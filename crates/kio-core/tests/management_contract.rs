use std::fs;

use kio_core::management::{
    CASE_PROBE_BYTES, CASE_PROBE_LEAF, CHILD_INITIALIZATION_PENDING_ERROR,
    CHILD_INITIALIZATION_PENDING_LEAF, ChildEnrollment, DirectoryIdentity, ManagementAuthority,
    ManagementBinding, ROOT_INITIALIZATION_PENDING_ERROR, ROOT_INITIALIZATION_PENDING_LEAF,
    begin_registration, cleanup_case_probe_in_directory, compare_and_remove_child_enrollment,
    detect_case_insensitive, detect_case_insensitive_in_directory, directory_identity_from_handle,
    enroll_child, finish_registration, initialize_child, initialize_root, observe_direct_child,
    peek_registration_marker, planned_child_management_record, publish_registration_record,
    read_record, read_registration_recovery_record, recorded_child_root, validate_case_probe,
    validate_live_chain, write_planned_child_management_record,
};
#[cfg(unix)]
use kio_core::management::{
    CONTROLLED_ROOT_ERROR, validate_controlled_root, validate_prospective_child,
};
use kio_core::scope::Repository;
use kio_core::store_dir::StoreDirectory;

fn scope(parent: &std::path::Path, name: &str) -> ManagementBinding {
    let root = parent.join(name);
    fs::create_dir_all(&root).unwrap();
    Repository::init(&root).unwrap();
    ManagementBinding::bind(root.canonicalize().unwrap()).unwrap()
}

fn scope_id(binding: &ManagementBinding) -> String {
    serde_json::from_slice::<serde_json::Value>(
        &fs::read(binding.canonical_root().join(".kio/scope.json")).unwrap(),
    )
    .unwrap()["scope_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[cfg(unix)]
#[test]
fn controlled_root_admits_safe_shared_read_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("shared-read-root");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    Repository::init(&path).unwrap();
    let canonical = path.canonicalize().unwrap();
    let root = fs::File::open(&canonical).unwrap();
    validate_controlled_root(&root, &canonical).unwrap();
    assert!(ManagementBinding::bind(canonical).is_ok());
}

#[cfg(unix)]
#[test]
fn captured_root_rejects_later_group_or_world_writes_without_store_mutation() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("root");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    Repository::init(&path).unwrap();
    let canonical = path.canonicalize().unwrap();
    let binding = ManagementBinding::bind(&canonical).unwrap();
    let management = canonical.join(".kio/management.json");
    for mode in [0o770, 0o777] {
        fs::set_permissions(&canonical, fs::Permissions::from_mode(mode)).unwrap();
        let error = binding.revalidate().unwrap_err();
        assert_eq!(error.error_code(), CONTROLLED_ROOT_ERROR);
        assert!(!management.exists());
        fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn unsafe_direct_child_is_rejected_before_journal_or_probe() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let parent = scope(temp.path(), "parent");
    initialize_root(&parent, scope_id(&parent), false).unwrap();
    let child = parent.canonical_root().join("child");
    fs::create_dir(&child).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o777)).unwrap();
    let error = observe_direct_child(&parent, "child").err().unwrap();
    assert_eq!(error.error_code(), CONTROLLED_ROOT_ERROR);
    let retained_child = fs::File::open(&child).unwrap();
    let error = validate_prospective_child(&parent, &retained_child, &child).unwrap_err();
    assert_eq!(error.error_code(), CONTROLLED_ROOT_ERROR);
    assert!(!child.join(".kio").exists());
    assert!(!child.join(CASE_PROBE_LEAF).exists());
    assert!(peek_registration_marker(&parent).unwrap().is_none());
}

#[test]
fn root_child_grandchild_requires_a_complete_reciprocal_chain() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let root_id = scope_id(&root);
    initialize_root(&root, &root_id, false).unwrap();

    let child = scope(root.canonical_root(), "child");
    let child_id = scope_id(&child);
    enroll_child(
        &root,
        "child",
        &child_id,
        "child-token",
        child.directory_identity(),
    )
    .unwrap();
    initialize_child(&root, &child, &child_id, "child-token").unwrap();

    let grandchild = scope(child.canonical_root(), "grandchild");
    let grandchild_id = scope_id(&grandchild);
    enroll_child(
        &child,
        "grandchild",
        &grandchild_id,
        "grandchild-token",
        grandchild.directory_identity(),
    )
    .unwrap();
    initialize_child(&child, &grandchild, &grandchild_id, "grandchild-token").unwrap();

    let chain = validate_live_chain(&grandchild).unwrap();
    assert_eq!(chain.scopes.len(), 3);
    assert_eq!(chain.scopes[0].record.scope_id, grandchild_id);
    assert_eq!(chain.scopes[2].record.scope_id, root_id);
    assert!(!chain.digest_input.is_empty());
}

#[test]
fn absent_parent_record_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let child = scope(root.canonical_root(), "child");
    let error = initialize_child(&root, &child, "child-id", "token").unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-MANAGEMENT-AUTHORITY-001");
}

#[test]
fn mismatched_parent_token_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let child = scope(root.canonical_root(), "child");
    let child_id = scope_id(&child);
    enroll_child(
        &root,
        "child",
        &child_id,
        "correct",
        child.directory_identity(),
    )
    .unwrap();
    assert!(initialize_child(&root, &child, &child_id, "wrong").is_err());
}

#[test]
fn moved_and_copied_management_records_do_not_enroll_a_new_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();

    let moved = temp.path().join("moved");
    let original = root.canonical_root().to_path_buf();
    // Model an external move while no process retains the scope's handles.
    drop(root);
    fs::rename(&original, &moved).unwrap();
    let moved_binding = ManagementBinding::bind(moved.canonicalize().unwrap()).unwrap();
    assert!(read_record(&moved_binding).is_err());

    let copy = temp.path().join("copy");
    fs::create_dir_all(copy.join(".kio")).unwrap();
    fs::copy(
        moved.join(".kio/management.json"),
        copy.join(".kio/management.json"),
    )
    .unwrap();
    let copy_binding = ManagementBinding::bind(copy.canonicalize().unwrap()).unwrap();
    assert!(read_record(&copy_binding).is_err());
}

#[test]
fn retained_binding_rejects_a_replaced_canonical_root_entry() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let record = initialize_root(&root, scope_id(&root), false).unwrap();
    let old = temp.path().join("old-root");
    let rename = fs::rename(root.canonical_root(), &old);
    #[cfg(windows)]
    {
        // cap-primitives omits FILE_SHARE_DELETE on retained directories, so
        // Windows prevents the replacement itself while this binding is live.
        assert_eq!(rename.unwrap_err().raw_os_error(), Some(32));
        assert!(!old.exists());
        root.revalidate().unwrap();
        let chain = validate_live_chain(&root).unwrap();
        assert_eq!(chain.scopes.len(), 1);
        assert_eq!(chain.scopes[0].record, record);
        assert_eq!(
            chain.scopes[0].binding.directory_identity(),
            root.directory_identity()
        );
    }
    #[cfg(not(windows))]
    {
        let _ = record;
        rename.unwrap();
        fs::create_dir_all(temp.path().join("root")).unwrap();
        Repository::init(temp.path().join("root")).unwrap();
        assert!(validate_live_chain(&root).is_err());
    }
}

#[test]
fn malformed_and_duplicate_or_cyclic_records_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let path = root.canonical_root().join(".kio/management.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["unexpected"] = serde_json::json!(true);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read_record(&root).is_err());

    let second = scope(temp.path(), "second");
    initialize_root(&second, scope_id(&second), false).unwrap();
    let parent = scope(temp.path(), "parent");
    let parent_id = scope_id(&parent);
    initialize_root(&parent, &parent_id, false).unwrap();
    let child = scope(parent.canonical_root(), "child");
    let child_id = scope_id(&child);
    enroll_child(
        &parent,
        "child",
        &child_id,
        "token",
        child.directory_identity(),
    )
    .unwrap();
    initialize_child(&parent, &child, &child_id, "token").unwrap();
    let child_path = child.canonical_root().join(".kio/management.json");
    let mut child_value: serde_json::Value =
        serde_json::from_slice(&fs::read(&child_path).unwrap()).unwrap();
    child_value["authority"] = serde_json::to_value(ManagementAuthority::Child {
        parent_scope_id: parent_id.clone(),
        root_scope_id: parent_id.clone(),
        enrollment_token: "token".into(),
    })
    .unwrap();
    child_value["scope_id"] = serde_json::json!(parent_id);
    fs::write(&child_path, serde_json::to_vec(&child_value).unwrap()).unwrap();
    assert!(validate_live_chain(&child).is_err());
}

#[test]
fn competing_root_creation_is_create_only_and_enrollment_conflicts_require_revoke() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let one = root.clone();
    let two = root.clone();
    let id = scope_id(&root);
    let first = std::thread::spawn(move || initialize_root(&one, id, false).is_ok());
    let second_id = scope_id(&root);
    let second = std::thread::spawn(move || initialize_root(&two, second_id, false).is_ok());
    assert_ne!(first.join().unwrap(), second.join().unwrap());

    let child = scope(root.canonical_root(), "child");
    let child_id = scope_id(&child);
    enroll_child(
        &root,
        "child",
        &child_id,
        "first",
        child.directory_identity(),
    )
    .unwrap();
    assert!(
        enroll_child(
            &root,
            "child",
            &child_id,
            "second",
            child.directory_identity(),
        )
        .is_err()
    );
    enroll_child(
        &root,
        "child",
        &child_id,
        "first",
        child.directory_identity(),
    )
    .unwrap();
}

#[test]
fn observation_needs_no_child_kio_and_enrollment_pins_the_expected_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let child_path = root.canonical_root().join("child");
    fs::create_dir(&child_path).unwrap();
    let observed = observe_direct_child(&root, "child").unwrap().unwrap();
    assert!(!child_path.join(".kio").exists());

    Repository::init(&child_path).unwrap();
    let child = ManagementBinding::bind(child_path.canonicalize().unwrap()).unwrap();
    let child_id = scope_id(&child);
    enroll_child(
        &root,
        "child",
        &child_id,
        "pinned-token",
        &observed.directory_identity,
    )
    .unwrap();
    // Enrollment persists the identity independently of an open child binding.
    drop(child);
    fs::remove_dir_all(child_path.join(".kio")).unwrap();
    enroll_child(
        &root,
        "child",
        &child_id,
        "pinned-token",
        &observed.directory_identity,
    )
    .unwrap();

    let observed_identity = observed.directory_identity.clone();
    drop(observed);
    fs::rename(&child_path, root.canonical_root().join("old-child")).unwrap();
    fs::create_dir(&child_path).unwrap();
    Repository::init(&child_path).unwrap();
    let replacement = ManagementBinding::bind(child_path.canonicalize().unwrap()).unwrap();
    assert!(
        enroll_child(
            &root,
            "child",
            scope_id(&replacement),
            "pinned-token",
            &observed_identity,
        )
        .is_err()
    );
    assert!(
        enroll_child(
            &root,
            "child",
            scope_id(&replacement),
            "pinned-token",
            replacement.directory_identity(),
        )
        .is_err()
    );
}

#[test]
fn exact_enrollment_removal_never_treats_a_conflict_as_completed() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let child = scope(root.canonical_root(), "child");
    let child_id = scope_id(&child);
    enroll_child(
        &root,
        "child",
        &child_id,
        "retire-token",
        child.directory_identity(),
    )
    .unwrap();
    let expected = read_record(&root).unwrap().children["child"].clone();
    assert!(compare_and_remove_child_enrollment(&root, "child", &expected).unwrap());
    assert!(!compare_and_remove_child_enrollment(&root, "child", &expected).unwrap());

    enroll_child(
        &root,
        "child",
        &child_id,
        "replacement-token",
        child.directory_identity(),
    )
    .unwrap();
    assert!(compare_and_remove_child_enrollment(&root, "child", &expected).is_err());
}

#[test]
fn management_mutations_share_a_held_repository_store_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let child = scope(root.canonical_root(), "child");
    let child_id = scope_id(&child);
    let repository = Repository::open(root.canonical_root()).unwrap();
    let _held_store_lock = repository.lock_store().unwrap();

    let observed = observe_direct_child(&root, "child").unwrap().unwrap();
    assert!(planned_child_management_record(&root, &observed, &child_id, "planned-token").is_ok());
    enroll_child(
        &root,
        "child",
        &child_id,
        "locked-token",
        child.directory_identity(),
    )
    .unwrap();
    let expected = read_record(&root).unwrap().children["child"].clone();
    assert!(compare_and_remove_child_enrollment(&root, "child", &expected).unwrap());
}

#[test]
fn child_enrollment_identity_is_required_and_cross_platform_serializable() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let child = scope(root.canonical_root(), "child");
    let child_id = scope_id(&child);
    enroll_child(
        &root,
        "child",
        &child_id,
        "required-identity",
        child.directory_identity(),
    )
    .unwrap();
    let path = root.canonical_root().join(".kio/management.json");
    let original = fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value["children"]["child"]
        .as_object_mut()
        .unwrap()
        .remove("directory_identity");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read_record(&root).is_err());

    let mut cross_volume: serde_json::Value = serde_json::from_slice(&original).unwrap();
    cross_volume["children"]["child"]["directory_identity"] =
        serde_json::to_value(foreign_directory_identity()).unwrap();
    fs::write(&path, serde_json::to_vec(&cross_volume).unwrap()).unwrap();
    assert!(read_record(&root).is_err());

    let foreign = ChildEnrollment {
        scope_id: "foreign-scope".into(),
        enrollment_token: "foreign-token".into(),
        directory_identity: DirectoryIdentity::Windows {
            volume_serial_number: 1,
            file_index: 2,
        },
    };
    let bytes = serde_json::to_vec(&foreign).unwrap();
    assert_eq!(
        serde_json::from_slice::<ChildEnrollment>(&bytes).unwrap(),
        foreign
    );
}

#[test]
fn planned_child_record_writes_to_stage_without_parent_enrollment() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let child_path = root.canonical_root().join("child");
    fs::create_dir(&child_path).unwrap();
    let child_repository = Repository::init(&child_path).unwrap();
    let child_scope_id = serde_json::from_slice::<serde_json::Value>(
        &fs::read(child_repository.kio_dir().join("scope.json")).unwrap(),
    )
    .unwrap()["scope_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let observed = observe_direct_child(&root, "child").unwrap().unwrap();
    assert_eq!(
        directory_identity_from_handle(&observed.handle).unwrap(),
        observed.directory_identity.clone()
    );
    let child_directory = StoreDirectory::open(&child_path).unwrap();
    let child_owner = StoreDirectory::open(child_repository.kio_dir()).unwrap();
    let _case_insensitive =
        detect_case_insensitive_in_directory(&child_directory, &child_owner).unwrap();
    // A planning-only call must not acquire a writer lock or recreate its
    // archive directories, including on a device where they are absent.
    let internal = root.canonical_root().join(".kio/internal");
    if internal.exists() {
        fs::remove_dir_all(&internal).unwrap();
    }
    let staged =
        planned_child_management_record(&root, &observed, child_scope_id, "stage-token").unwrap();
    assert!(!internal.exists());
    assert!(!root.canonical_root().join(".kio/.lock").exists());
    assert!(read_record(&root).unwrap().children.is_empty());
    let stage = StoreDirectory::open(child_repository.kio_dir()).unwrap();
    write_planned_child_management_record(&stage, &staged).unwrap();
    assert!(write_planned_child_management_record(&stage, &staged).is_err());
}

#[test]
fn hard_linked_management_record_is_rejected_and_case_probe_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let record = root.canonical_root().join(".kio/management.json");
    fs::hard_link(&record, root.canonical_root().join(".kio/management-copy")).unwrap();
    assert!(read_record(&root).is_err());
    fs::remove_file(root.canonical_root().join(".kio/management-copy")).unwrap();
    let _case_insensitive = detect_case_insensitive(&root).unwrap();
}

#[test]
fn binding_case_probe_uses_actual_root_with_private_control_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let _case_insensitive = detect_case_insensitive(&root).unwrap();
    assert!(!root.canonical_root().join(".kio-atomic").exists());
    assert!(!root.canonical_root().join(CASE_PROBE_LEAF).exists());
    assert!(root.canonical_root().join(".kio/.kio-atomic").is_dir());
}

#[test]
fn interrupted_case_probe_is_reused_and_tampered_probe_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let control = StoreDirectory::open(&root.canonical_root().join(".kio")).unwrap();
    control
        .write_atomic(
            std::path::Path::new(CASE_PROBE_LEAF),
            CASE_PROBE_BYTES,
            kio_core::store_dir::Publication::CreateOnly,
        )
        .unwrap();
    let _case_insensitive = detect_case_insensitive_in_directory(&control, &control).unwrap();
    assert!(
        control
            .read_optional(
                std::path::Path::new(CASE_PROBE_LEAF),
                CASE_PROBE_BYTES.len() as u64,
            )
            .unwrap()
            .is_none()
    );

    control
        .write_atomic(
            std::path::Path::new(CASE_PROBE_LEAF),
            b"tampered",
            kio_core::store_dir::Publication::CreateOnly,
        )
        .unwrap();
    assert!(validate_case_probe(&control).is_err());
    assert!(detect_case_insensitive_in_directory(&control, &control).is_err());
}

#[test]
fn case_probe_cleanup_only_removes_an_exact_existing_probe() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let control = StoreDirectory::open(&root.canonical_root().join(".kio")).unwrap();
    control
        .write_atomic(
            std::path::Path::new(CASE_PROBE_LEAF),
            CASE_PROBE_BYTES,
            kio_core::store_dir::Publication::CreateOnly,
        )
        .unwrap();
    assert!(cleanup_case_probe_in_directory(&control, &control).unwrap());
    assert!(!cleanup_case_probe_in_directory(&control, &control).unwrap());
}

#[cfg(unix)]
#[test]
fn case_probe_distinguishes_an_alias_from_a_distinct_uppercase_leaf() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    let control = StoreDirectory::open(&root.canonical_root().join(".kio")).unwrap();
    control
        .write_atomic(
            std::path::Path::new(CASE_PROBE_LEAF),
            CASE_PROBE_BYTES,
            kio_core::store_dir::Publication::CreateOnly,
        )
        .unwrap();
    match control.write_atomic(
        std::path::Path::new(".KIO-CASE-PROBE"),
        CASE_PROBE_BYTES,
        kio_core::store_dir::Publication::CreateOnly,
    ) {
        Ok(()) => assert!(validate_case_probe(&control).is_err()),
        Err(error) => {
            assert_eq!(
                error
                    .context()
                    .get("io_error_kind")
                    .and_then(|value| value.as_str()),
                Some("already_exists")
            );
            use std::os::unix::fs::MetadataExt;
            let lower = control
                .open_regular_read(std::path::Path::new(CASE_PROBE_LEAF), 1024)
                .unwrap()
                .metadata()
                .unwrap();
            let upper = control
                .open_regular_read(std::path::Path::new(".KIO-CASE-PROBE"), 1024)
                .unwrap()
                .metadata()
                .unwrap();
            assert_eq!((lower.dev(), lower.ino()), (upper.dev(), upper.ino()));
            assert!(validate_case_probe(&control).unwrap());
        }
    }
}

#[test]
fn pending_registration_blocks_normal_reads_but_recovery_survives_a_move() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let record = initialize_root(&root, scope_id(&root), false).unwrap();
    begin_registration(&root, "move-1").unwrap();
    assert_eq!(
        peek_registration_marker(&root).unwrap().as_deref(),
        Some("move-1")
    );
    assert_eq!(
        read_record(&root).unwrap_err().error_code(),
        "KIO-E-REGISTRATION-PENDING-001"
    );

    let moved = temp.path().join("moved");
    let original = root.canonical_root().to_path_buf();
    // Recovery opens fresh handles after the interrupted process has exited.
    drop(root);
    fs::rename(&original, &moved).unwrap();
    let moved_binding = ManagementBinding::bind(moved.canonicalize().unwrap()).unwrap();
    assert_eq!(
        read_registration_recovery_record(&moved_binding).unwrap(),
        record
    );
    assert!(read_record(&moved_binding).is_err());
}

#[test]
fn child_initialization_marker_blocks_normal_reads_but_recovery_reader_remains_available() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let record = initialize_root(&root, scope_id(&root), false).unwrap();
    fs::write(
        root.canonical_root()
            .join(".kio")
            .join(CHILD_INITIALIZATION_PENDING_LEAF),
        b"parent recovery owns this child",
    )
    .unwrap();

    assert_eq!(
        read_record(&root).unwrap_err().error_code(),
        CHILD_INITIALIZATION_PENDING_ERROR
    );
    assert_eq!(
        validate_live_chain(&root).unwrap_err().error_code(),
        CHILD_INITIALIZATION_PENDING_ERROR
    );
    assert_eq!(read_registration_recovery_record(&root).unwrap(), record);
}

#[test]
fn root_initialization_marker_blocks_normal_reads_but_recovery_reader_remains_available() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let record = initialize_root(&root, scope_id(&root), false).unwrap();
    fs::write(
        root.canonical_root()
            .join(".kio")
            .join(ROOT_INITIALIZATION_PENDING_LEAF),
        b"root bootstrap owns this scope",
    )
    .unwrap();

    assert_eq!(
        read_record(&root).unwrap_err().error_code(),
        ROOT_INITIALIZATION_PENDING_ERROR
    );
    assert_eq!(
        validate_live_chain(&root).unwrap_err().error_code(),
        ROOT_INITIALIZATION_PENDING_ERROR
    );
    assert_eq!(read_registration_recovery_record(&root).unwrap(), record);
}

#[test]
fn recovery_reader_rejects_relative_or_traversing_recorded_locations() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    initialize_root(&root, scope_id(&root), false).unwrap();
    begin_registration(&root, "move-1").unwrap();
    let path = root.canonical_root().join(".kio/management.json");

    for canonical_root in ["relative", "/tmp/../invalid"] {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["canonical_root"] = serde_json::json!(canonical_root);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_registration_recovery_record(&root).is_err());
    }
}

#[test]
fn recovery_reads_foreign_directory_identity_but_normal_read_requires_live_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let before = initialize_root(&root, scope_id(&root), false).unwrap();

    let mut foreign_before = before.clone();
    foreign_before.directory_identity = foreign_directory_identity();
    foreign_before.canonical_root = foreign_recorded_root();
    let path = root.canonical_root().join(".kio/management.json");
    fs::write(&path, serde_json::to_vec(&foreign_before).unwrap()).unwrap();

    assert_eq!(
        read_registration_recovery_record(&root).unwrap(),
        foreign_before
    );
    assert!(read_record(&root).is_err());

    begin_registration(&root, "cross-platform-move").unwrap();
    let mut after = before;
    after.registration_generation += 1;
    publish_registration_record(&root, "cross-platform-move", &foreign_before, &after).unwrap();
    finish_registration(&root, "cross-platform-move", &after).unwrap();
    assert_eq!(read_record(&root).unwrap(), after);
}

#[test]
fn recovery_rejects_invalid_foreign_recorded_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let before = initialize_root(&root, scope_id(&root), false).unwrap();
    begin_registration(&root, "cross-platform-invalid").unwrap();
    let path = root.canonical_root().join(".kio/management.json");

    for canonical_root in invalid_foreign_recorded_roots() {
        let mut foreign = before.clone();
        foreign.directory_identity = foreign_directory_identity();
        foreign.canonical_root = std::path::PathBuf::from(canonical_root);
        fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert!(read_registration_recovery_record(&root).is_err());
    }
}

#[test]
fn recorded_child_root_uses_the_recorded_platform_separator() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let mut record = initialize_root(&root, scope_id(&root), false).unwrap();
    record.directory_identity = foreign_directory_identity();
    record.canonical_root = foreign_recorded_root();

    #[cfg(unix)]
    assert_eq!(
        recorded_child_root(&record, "child").unwrap(),
        std::path::Path::new(r"C:\moved-root\child")
    );
    #[cfg(windows)]
    assert_eq!(
        recorded_child_root(&record, "child").unwrap(),
        std::path::Path::new("/moved-root/child")
    );
}

#[cfg(unix)]
#[test]
fn recovery_accepts_a_verbatim_windows_recorded_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let before = initialize_root(&root, scope_id(&root), false).unwrap();
    begin_registration(&root, "cross-platform-verbatim").unwrap();
    let mut foreign = before;
    foreign.directory_identity = DirectoryIdentity::Windows {
        volume_serial_number: 1,
        file_index: 2,
    };
    foreign.canonical_root = std::path::PathBuf::from(r"\\?\C:\moved-root");
    fs::write(
        root.canonical_root().join(".kio/management.json"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();
    assert_eq!(read_registration_recovery_record(&root).unwrap(), foreign);
    assert!(read_record(&root).is_err());
}

fn foreign_directory_identity() -> DirectoryIdentity {
    #[cfg(unix)]
    {
        DirectoryIdentity::Windows {
            volume_serial_number: 1,
            file_index: 2,
        }
    }
    #[cfg(windows)]
    {
        DirectoryIdentity::Unix {
            device: 1,
            inode: 2,
        }
    }
}

fn foreign_recorded_root() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::path::PathBuf::from(r"C:\moved-root")
    }
    #[cfg(windows)]
    {
        std::path::PathBuf::from("/moved-root")
    }
}

fn invalid_foreign_recorded_roots() -> &'static [&'static str] {
    #[cfg(unix)]
    {
        &[
            r"C:relative",
            r"C:/slash",
            r"\\server\share",
            r"C:\double\\separator",
            r"C:\trailing\",
            r"C:\.\child",
            r"C:\..\child",
        ]
    }
    #[cfg(windows)]
    {
        &[
            "relative",
            "//server/share",
            "/double//separator",
            "/trailing/",
            "/./child",
            "/../child",
        ]
    }
}

#[test]
fn registration_publish_is_idempotent_and_requires_the_matching_marker() {
    let temp = tempfile::tempdir().unwrap();
    let root = scope(temp.path(), "root");
    let before = initialize_root(&root, scope_id(&root), false).unwrap();
    let repository = Repository::open(root.canonical_root()).unwrap();
    let _held_session_lock = repository.lock_store().unwrap();
    begin_registration(&root, "operation-1").unwrap();
    begin_registration(&root, "operation-1").unwrap();
    assert!(begin_registration(&root, "other-operation").is_err());

    let mut after = before.clone();
    after.case_insensitive = true;
    after.registration_generation += 1;
    assert!(publish_registration_record(&root, "wrong", &before, &after).is_err());
    publish_registration_record(&root, "operation-1", &before, &after).unwrap();
    publish_registration_record(&root, "operation-1", &before, &after).unwrap();
    assert!(finish_registration(&root, "wrong", &after).is_err());
    finish_registration(&root, "operation-1", &after).unwrap();
    assert_eq!(read_record(&root).unwrap(), after);
}
