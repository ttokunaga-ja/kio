use std::fs;
use std::path::Path;

use kio_core::management::{ManagementBinding, enroll_child, initialize_child, initialize_root};
use kio_core::scope::Repository;
use kio_pipeline::scan::{
    CHILD_SCOPE_DISCOVERY_PAGE_SIZE, discover_managed_child_scopes_page,
    parse_child_scope_discovery_continuation,
};

fn managed_root(path: &Path) -> ManagementBinding {
    fs::create_dir_all(path).unwrap();
    let canonical = path.canonicalize().unwrap();
    let repository = Repository::init(&canonical).unwrap();
    let binding = ManagementBinding::bind(&canonical).unwrap();
    let scope: serde_json::Value =
        serde_json::from_slice(&fs::read(repository.kio_dir().join("scope.json")).unwrap())
            .unwrap();
    initialize_root(
        &binding,
        scope["scope_id"].as_str().unwrap().to_owned(),
        false,
    )
    .unwrap();
    binding
}

#[test]
fn pages_all_children_without_the_legacy_directory_cap() {
    let temp = tempfile::tempdir().unwrap();
    let binding = managed_root(&temp.path().join("root"));
    for index in 0..600 {
        fs::create_dir(binding.canonical_root().join(format!("child-{index:04}"))).unwrap();
    }
    let mut continuation = None;
    let mut found = Vec::new();
    loop {
        let slice = discover_managed_child_scopes_page(&binding, continuation.as_ref()).unwrap();
        assert!(slice.plan.candidates.len() <= CHILD_SCOPE_DISCOVERY_PAGE_SIZE);
        found.extend(
            slice
                .plan
                .candidates
                .iter()
                .filter(|row| row.status == "planned")
                .map(|row| row.path.clone()),
        );
        continuation = slice.continuation;
        if continuation.is_none() {
            break;
        }
    }
    assert_eq!(found.len(), 600);
    assert_eq!(found.first().map(String::as_str), Some("child-0000"));
    assert_eq!(found.last().map(String::as_str), Some("child-0599"));
}

#[test]
fn continuation_rejects_a_policy_change() {
    let temp = tempfile::tempdir().unwrap();
    let binding = managed_root(&temp.path().join("root"));
    for index in 0..130 {
        fs::create_dir(binding.canonical_root().join(format!("child-{index:04}"))).unwrap();
    }
    let slice = discover_managed_child_scopes_page(&binding, None).unwrap();
    let continuation = slice.continuation.expect("requires a second page");
    fs::write(binding.canonical_root().join(".kioignore"), "child-0129/\n").unwrap();
    let error = match discover_managed_child_scopes_page(&binding, Some(&continuation)) {
        Ok(_) => panic!("changed policy must invalidate continuation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001")
    );
}

#[test]
fn deep_frontier_continues_after_enrollment_without_policy_starvation() {
    let temp = tempfile::tempdir().unwrap();
    let binding = managed_root(&temp.path().join("root"));
    let mut current = binding.canonical_root().to_path_buf();
    for index in 0..64 {
        current = current.join(format!("d{index:02}"));
        fs::create_dir(&current).unwrap();
    }
    // Force a second slice after the deep DFS path so enrollment occurs while
    // a parent continuation is still pending.
    for index in 0..130 {
        fs::create_dir(binding.canonical_root().join(format!("z{index:04}"))).unwrap();
    }
    let first = discover_managed_child_scopes_page(&binding, None).unwrap();
    let first_child = first
        .plan
        .candidates
        .iter()
        .find(|row| row.status == "planned")
        .expect("first child")
        .path
        .clone();
    let child_path = binding.canonical_root().join(&first_child);
    let child_repository = Repository::init(&child_path).unwrap();
    let child_binding = ManagementBinding::bind(&child_path).unwrap();
    let child_scope: serde_json::Value =
        serde_json::from_slice(&fs::read(child_repository.kio_dir().join("scope.json")).unwrap())
            .unwrap();
    let child_id = child_scope["scope_id"].as_str().unwrap().to_owned();
    enroll_child(
        &binding,
        &first_child,
        &child_id,
        "page-test-child",
        child_binding.directory_identity(),
    )
    .unwrap();
    initialize_child(&binding, &child_binding, child_id, "page-test-child").unwrap();

    let mut continuation = first.continuation;
    assert!(
        continuation.is_some(),
        "wide sibling set requires resume after enrollment"
    );
    let deep_relative = current
        .strip_prefix(binding.canonical_root())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    assert_eq!(deep_relative.split('/').count(), 64);
    let mut resumed_children = 0;
    while let Some(token) = continuation {
        let slice = discover_managed_child_scopes_page(&binding, Some(&token)).unwrap();
        resumed_children += slice
            .plan
            .candidates
            .iter()
            .filter(|row| row.status == "planned")
            .count();
        continuation = slice.continuation;
    }
    assert_eq!(resumed_children, 66);
}

#[test]
fn untrusted_continuation_rejects_unknown_fields_and_invalid_names_before_resume() {
    let temp = tempfile::tempdir().unwrap();
    let binding = managed_root(&temp.path().join("root"));
    for index in 0..130 {
        fs::create_dir(binding.canonical_root().join(format!("child-{index:04}"))).unwrap();
    }
    let token = discover_managed_child_scopes_page(&binding, None)
        .unwrap()
        .continuation
        .unwrap();
    let mut unknown = serde_json::to_value(&token).unwrap();
    unknown["unknown"] = serde_json::json!(true);
    assert!(
        parse_child_scope_discovery_continuation(&serde_json::to_vec(&unknown).unwrap()).is_err()
    );

    let mut invalid_name = serde_json::to_value(&token).unwrap();
    invalid_name["frames"][0]["after_entry"] = serde_json::json!("../escape");
    assert!(
        parse_child_scope_discovery_continuation(&serde_json::to_vec(&invalid_name).unwrap())
            .is_err()
    );
    let mut duplicate_frame = serde_json::to_value(&token).unwrap();
    let first_frame = duplicate_frame["frames"][0].clone();
    duplicate_frame["frames"]
        .as_array_mut()
        .unwrap()
        .push(first_frame);
    assert!(
        parse_child_scope_discovery_continuation(&serde_json::to_vec(&duplicate_frame).unwrap())
            .is_err()
    );
    let mut oversized_page = serde_json::to_value(&token).unwrap();
    oversized_page["frames"][0]["pending_entries"] = serde_json::json!(
        (0..130)
            .map(|index| format!("name-{index:03}"))
            .collect::<Vec<_>>()
    );
    assert!(
        parse_child_scope_discovery_continuation(&serde_json::to_vec(&oversized_page).unwrap())
            .is_err()
    );
    let oversized = vec![b'x'; 1024 * 1024 + 1];
    assert!(parse_child_scope_discovery_continuation(&oversized).is_err());
}
