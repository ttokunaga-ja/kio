use std::fs;
use std::path::Path;

use kio_core::management::{ManagementBinding, enroll_child, initialize_child, initialize_root};
use kio_core::scope::Repository;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::scan::{
    ScanPreviewRequest, bind_planned_child, build_managed_scan_preview,
    build_planned_child_scan_preview, discover_managed_child_scopes,
    managed_scan_policy_allows_file,
};

fn scope(path: &Path) -> (Repository, ManagementBinding, String) {
    fs::create_dir_all(path).unwrap();
    let canonical = path.canonicalize().unwrap();
    let repository = Repository::init(&canonical).unwrap();
    let binding = ManagementBinding::bind(&canonical).unwrap();
    let scope: serde_json::Value =
        serde_json::from_slice(&fs::read(canonical.join(".kio/scope.json")).unwrap()).unwrap();
    let id = scope["scope_id"].as_str().unwrap().to_owned();
    (repository, binding, id)
}

fn child(parent: &Path, name: &str, case_insensitive: bool) -> ManagementBinding {
    let (_, parent_binding, parent_id) = scope(parent);
    initialize_root(&parent_binding, parent_id, case_insensitive).unwrap();
    let child_path = parent.join(name);
    let (_, child_binding, child_id) = scope(&child_path);
    enroll_child(
        &parent_binding,
        name,
        &child_id,
        "child-token",
        child_binding.directory_identity(),
    )
    .unwrap();
    initialize_child(&parent_binding, &child_binding, child_id, "child-token").unwrap();
    child_binding
}

#[test]
fn parent_deny_survives_child_negation_and_applies_to_deleted_history_names() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let target = child(&parent, "child", false);
    fs::write(parent.join(".kioignore"), "private.md\n").unwrap();
    fs::write(target.canonical_root().join(".kioignore"), "!private.md\n").unwrap();

    let policy = CurrentPolicyEvaluator::load(&target, false).unwrap();
    assert!(!policy.allows_path("private.md").unwrap());
    assert!(policy.allows_path("public.md").unwrap());
}

#[test]
fn grandparent_directory_deny_blocks_the_child_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let child_binding = child(&root, "child", false);
    let grandchild_path = root.join("child/grandchild");
    let (_, grandchild, grandchild_id) = scope(&grandchild_path);
    enroll_child(
        &child_binding,
        "grandchild",
        &grandchild_id,
        "grandchild-token",
        grandchild.directory_identity(),
    )
    .unwrap();
    initialize_child(
        &child_binding,
        &grandchild,
        grandchild_id,
        "grandchild-token",
    )
    .unwrap();
    fs::write(root.join(".kioignore"), "child/grandchild/\n").unwrap();

    let policy = CurrentPolicyEvaluator::load(&grandchild, false).unwrap();
    assert!(!policy.allows_scope().unwrap());
    assert!(!policy.allows_path("history-deleted.md").unwrap());
}

#[test]
fn tier_a_needs_local_unignore_and_control_paths_are_never_allowed() {
    let temp = tempfile::tempdir().unwrap();
    let target = child(&temp.path().join("root"), "child", false);
    fs::write(target.canonical_root().join(".kioignore"), "!token.txt\n").unwrap();
    let policy = CurrentPolicyEvaluator::load(&target, false).unwrap();
    assert!(policy.allows_path("token.txt").unwrap());
    assert!(!policy.allows_path(".env").unwrap());
    assert!(!policy.allows_path(".kio/config.toml").unwrap());
    assert!(!policy.allows_path(".kioignore").unwrap());
}

#[test]
fn digest_revalidation_rejects_mutated_local_authority() {
    let temp = tempfile::tempdir().unwrap();
    let target = child(&temp.path().join("root"), "child", false);
    let policy = CurrentPolicyEvaluator::load(&target, false).unwrap();
    fs::write(target.canonical_root().join(".kioignore"), "private.md\n").unwrap();
    assert!(policy.revalidate().is_err());
}

#[test]
fn missing_or_unsafe_config_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let target = child(&temp.path().join("root"), "child", false);
    let config = target.canonical_root().join(".kio/config.toml");
    let _ = fs::remove_file(&config);
    assert!(CurrentPolicyEvaluator::load(&target, false).is_err());

    let replacement = target.canonical_root().join("replacement.toml");
    fs::write(&replacement, "[scope]\nignore = []\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&replacement, &config).unwrap();
    #[cfg(unix)]
    assert!(CurrentPolicyEvaluator::load(&target, false).is_err());
}

#[test]
fn management_reenrollment_changes_the_snapshot_digest() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let target = child(&root, "child", false);
    let policy = CurrentPolicyEvaluator::load(&target, false).unwrap();
    let root_binding = ManagementBinding::bind(root.canonicalize().unwrap()).unwrap();
    let (_, extra_binding, extra_id) = scope(&root.join("extra"));
    enroll_child(
        &root_binding,
        "extra",
        extra_id,
        "extra-token",
        extra_binding.directory_identity(),
    )
    .unwrap();
    assert!(policy.revalidate().is_err());
}

#[test]
fn caller_cannot_override_the_persisted_case_capability() {
    let temp = tempfile::tempdir().unwrap();
    let target = child(&temp.path().join("root"), "child", false);
    assert!(CurrentPolicyEvaluator::load(&target, true).is_err());
}

#[test]
fn managed_preview_applies_parent_deny_before_opening_raw_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let target = child(&parent, "child", false);
    fs::write(parent.join(".kioignore"), "child/private.md\n").unwrap();
    fs::write(target.canonical_root().join(".kioignore"), "!private.md\n").unwrap();
    let source = temp.path().join("source.md");
    fs::write(&source, "private").unwrap();
    fs::hard_link(&source, target.canonical_root().join("private.md")).unwrap();

    let preview = build_managed_scan_preview(
        &target,
        ScanPreviewRequest {
            scope_path: "ignored-display-path".to_owned(),
            include_raw_hashes: true,
            require_network_approval: false,
        },
    )
    .unwrap();
    let candidate = preview
        .candidates
        .iter()
        .find(|candidate| candidate.input_path == "private.md")
        .unwrap();
    assert!(candidate.ignored);
    assert!(candidate.raw_hash.is_none());
}

#[test]
fn managed_preview_is_read_only_and_presend_policy_is_live() {
    let temp = tempfile::tempdir().unwrap();
    let target = child(&temp.path().join("root"), "child", false);
    fs::write(target.canonical_root().join("report.md"), "report").unwrap();
    let config = fs::read(target.canonical_root().join(".kio/config.toml")).unwrap();
    let ignore = fs::read(target.canonical_root().join(".kioignore")).unwrap_or_default();
    let _ = build_managed_scan_preview(
        &target,
        ScanPreviewRequest {
            scope_path: String::new(),
            include_raw_hashes: false,
            require_network_approval: false,
        },
    )
    .unwrap();
    assert_eq!(
        fs::read(target.canonical_root().join(".kio/config.toml")).unwrap(),
        config
    );
    assert_eq!(
        fs::read(target.canonical_root().join(".kioignore")).unwrap_or_default(),
        ignore
    );
    assert!(managed_scan_policy_allows_file(&target, "report.md").unwrap());
    fs::write(target.canonical_root().join(".kioignore"), "report.md\n").unwrap();
    assert!(!managed_scan_policy_allows_file(&target, "report.md").unwrap());
}

#[test]
fn planned_child_preview_uses_retained_rules_before_hashing() {
    let temp = tempfile::tempdir().unwrap();
    let root_path = temp.path().join("root");
    let (_, root, root_id) = scope(&root_path);
    initialize_root(&root, root_id, false).unwrap();
    let child_path = root.canonical_root().join("child");
    fs::create_dir(&child_path).unwrap();
    let source = temp.path().join("source.md");
    fs::write(&source, "private").unwrap();
    fs::hard_link(&source, child_path.join("private.md")).unwrap();
    fs::write(child_path.join("public.md"), "public").unwrap();
    fs::write(
        root.canonical_root().join(".kioignore"),
        "child/private.md\n",
    )
    .unwrap();
    fs::write(child_path.join(".kioignore"), "!private.md\n").unwrap();

    let plan = discover_managed_child_scopes(&root).unwrap();
    let bound = bind_planned_child(&plan, "child").unwrap().unwrap();
    let preview = build_planned_child_scan_preview(
        &bound,
        ScanPreviewRequest {
            scope_path: "display-only".to_owned(),
            include_raw_hashes: true,
            require_network_approval: false,
        },
        false,
    )
    .unwrap();
    let candidate = preview
        .candidates
        .iter()
        .find(|candidate| candidate.input_path == "private.md")
        .unwrap();
    assert!(candidate.ignored);
    assert!(candidate.raw_hash.is_none());
    assert!(!child_path.join(".kio").exists());
}
