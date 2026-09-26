//! Crash-cut recovery contracts.  The outer test never changes this process's
//! device environment; it runs the exact same test binary once with a private
//! XDG home, where the gated invocation performs the retained-state work.

use super::*;
use crate::management::{
    ExplicitRoot, binding_for_repo, initialize_explicit_root, reconcile_planned_child,
};
use kio_core::management::{begin_registration, read_record};
use kio_pipeline::scan::BoundPlannedChild;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GATE: &str = "KIO_ROOT_REGISTRATION_RECOVERY_CHILD";

fn canonical_tempdir() -> tempfile::TempDir {
    // Strict store ancestry checks require the resolved macOS temporary root.
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

fn child(path: &Path) -> BoundPlannedChild {
    let canonical_root = path.canonicalize().unwrap();
    let root = StoreDirectory::open(&canonical_root)
        .unwrap()
        .root_handle()
        .as_ref()
        .try_clone()
        .unwrap();
    BoundPlannedChild {
        canonical_root,
        root,
        inherited_rules: Vec::new(),
    }
}

fn run_isolated(name: &str) {
    if std::env::var_os(GATE).is_some() {
        return;
    }
    let temp = canonical_tempdir();
    let home = temp.path().join("device");
    for leaf in ["home", "data", "config", "cache", "tmp"] {
        fs::create_dir_all(home.join(leaf)).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for directory in std::iter::once(temp.path().to_path_buf())
            .chain(std::iter::once(home.clone()))
            .chain(["home", "data", "config", "cache", "tmp"].map(|leaf| home.join(leaf)))
        {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        child.env("PATH", path);
    }
    if let Some(value) = std::env::var_os("SystemRoot") {
        child.env("SystemRoot", value);
    }
    let status = child
        .arg("--exact")
        .arg(name)
        .arg("--nocapture")
        .env(GATE, "1")
        .env("HOME", home.join("home"))
        .env("USERPROFILE", home.join("home"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("TMPDIR", home.join("tmp"))
        .env("TEMP", home.join("tmp"))
        .env("TMP", home.join("tmp"))
        .status()
        .unwrap();
    assert!(status.success());
}

fn setup() -> (tempfile::TempDir, PathBuf, Vec<u8>, String, String) {
    let temp = canonical_tempdir();
    let old = temp.path().join("old");
    let child_path = old.join("child");
    fs::create_dir_all(&child_path).unwrap();
    fs::write(old.join("note.txt"), b"recovery knowledge").unwrap();
    let root = match initialize_explicit_root(&old).unwrap() {
        ExplicitRoot::Created(repo) | ExplicitRoot::Existing(repo) => repo,
    };
    reconcile_planned_child(&root, child(&child_path)).unwrap();
    let root_id = read_record(&binding_for_repo(&root).unwrap())
        .unwrap()
        .scope_id;
    let child_id =
        read_record(&ManagementBinding::bind(child_path.canonicalize().unwrap()).unwrap())
            .unwrap()
            .scope_id;
    let moved = temp.path().join("moved");
    fs::rename(&old, &moved).unwrap();
    let moved = moved.canonicalize().unwrap();
    (
        temp,
        moved,
        b"recovery knowledge".to_vec(),
        root_id,
        child_id,
    )
}

fn persist_cut(root: &Path, phase: Phase, remove_child_marker: bool) {
    let binding = ManagementBinding::bind(root).unwrap();
    let mut nodes = plan_new(root, binding).unwrap();
    for node in &mut nodes {
        let case = detect_case_insensitive(&node.binding).unwrap();
        node.after.case_insensitive = case;
    }
    let journal = Journal {
        version: 1,
        operation_id: new_ulid(root),
        target_root: root.display().to_string(),
        phase,
        nodes: nodes.iter().map(node_to_journal).collect(),
    };
    let bytes = encode(&journal).unwrap();
    let control = control_for(&nodes[0].binding).unwrap();
    control
        .write_atomic(Path::new(JOURNAL_LEAF), &bytes, Publication::CreateOnly)
        .unwrap();
    if phase == Phase::Prepared {
        begin_registration(&nodes[0].binding, &journal.operation_id).unwrap();
    } else {
        for node in &nodes {
            begin_registration(&node.binding, &journal.operation_id).unwrap();
        }
    }
    if matches!(phase, Phase::Revoked | Phase::Published | Phase::Finishing) {
        let published = if phase == Phase::Revoked {
            1
        } else {
            nodes.len()
        };
        for node in nodes.iter().take(published) {
            publish_registration_record(
                &node.binding,
                &journal.operation_id,
                &node.before,
                &node.after,
            )
            .unwrap();
        }
    }
    if matches!(phase, Phase::Published | Phase::Finishing) {
        let replacements = nodes
            .iter()
            .map(|node| ScopeRegistrationRebind {
                scope_id: node.before.scope_id.clone(),
                old_kio_paths: node.old_kio_paths.clone(),
                new_registration: RegistryEntry {
                    scope_id: node.before.scope_id.clone(),
                    kio_path: node
                        .binding
                        .canonical_root()
                        .join(".kio")
                        .display()
                        .to_string(),
                    root_path: node.binding.canonical_root().display().to_string(),
                    participates_in_global_search: false,
                    indexed: false,
                    last_seen_at: crate::now_utc_seconds(),
                },
            })
            .collect::<Vec<_>>();
        RegistryDb::open_default()
            .unwrap()
            .rebind_scope_registrations(&replacements)
            .unwrap();
    }
    if remove_child_marker {
        finish_registration(&nodes[1].binding, &journal.operation_id, &nodes[1].after).unwrap();
    }
}

fn assert_recovered(root: &Path, bytes: &[u8], root_id: &str, child_id: &str) {
    assert_eq!(fs::read(root.join("note.txt")).unwrap(), bytes);
    let root_binding = ManagementBinding::bind(root).unwrap();
    let child_binding = ManagementBinding::bind(root.join("child")).unwrap();
    assert_eq!(read_record(&root_binding).unwrap().scope_id, root_id);
    assert_eq!(read_record(&child_binding).unwrap().scope_id, child_id);
    assert!(
        control_for(&root_binding)
            .unwrap()
            .read_optional(Path::new(JOURNAL_LEAF), MAX_JOURNAL_BYTES)
            .unwrap()
            .is_none()
    );
    assert!(peek_registration_marker(&root_binding).unwrap().is_none());
    assert!(peek_registration_marker(&child_binding).unwrap().is_none());
    assert_eq!(
        read_record(&root_binding).unwrap().registration_generation,
        2
    );
    assert_eq!(
        read_record(&child_binding).unwrap().registration_generation,
        2
    );
    let registry = RegistryDb::open_default_read_only().unwrap();
    for (id, path) in [
        (root_id, root.to_path_buf()),
        (child_id, root.join("child")),
    ] {
        let rows = registry.lookup_scope_id(id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(Path::new(&rows[0].root_path), path);
        assert!(!rows[0].indexed);
    }
}

fn preserved_bytes(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(
        root: &Path,
        relative: &Path,
        result: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) {
        assert!(relative.components().count() <= 64);
        for entry in fs::read_dir(root.join(relative)).unwrap() {
            let entry = entry.unwrap();
            let path = relative.join(entry.file_name());
            if [
                "management.json",
                JOURNAL_LEAF,
                "registration-pending.json",
                ".lock",
            ]
            .iter()
            .any(|excluded| path.file_name().unwrap() == *excluded)
            {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, result);
            } else {
                assert!(entry.file_type().unwrap().is_file());
                assert!(entry.metadata().unwrap().len() <= 8 * 1024 * 1024);
                assert!(result.len() < 10_000);
                result.insert(path, fs::read(entry.path()).unwrap());
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    visit(root, Path::new(""), &mut result);
    result
}

fn resume(root: &Path) {
    let before = preserved_bytes(root);
    let binding = ManagementBinding::bind(root).unwrap();
    let control = control_for(&binding).unwrap();
    let bytes = control
        .read_optional(Path::new(JOURNAL_LEAF), MAX_JOURNAL_BYTES)
        .unwrap()
        .unwrap();
    let mut journal = decode(&bytes, root).unwrap();
    execute(&binding, &mut journal, bytes, true).unwrap();
    assert_eq!(preserved_bytes(root), before);
}

#[test]
fn recovers_prepared_root_marker_cut() {
    let name = "root_registration::recovery_tests::recovers_prepared_root_marker_cut";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, bytes, root_id, child_id) = setup();
    persist_cut(&root, Phase::Prepared, false);
    resume(&root);
    assert_recovered(&root, &bytes, &root_id, &child_id);
}

#[test]
fn recovers_registration_with_an_absent_child_after_parent_publication() {
    let name = "root_registration::recovery_tests::recovers_registration_with_an_absent_child_after_parent_publication";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, bytes, root_id, _child_id) = setup();
    fs::remove_dir_all(root.join("child")).unwrap();
    persist_cut(&root, Phase::Revoked, false);
    resume(&root);
    let binding = ManagementBinding::bind(&root).unwrap();
    let record = read_record(&binding).unwrap();
    assert_eq!(record.scope_id, root_id);
    assert_eq!(record.registration_generation, 2);
    assert!(record.children.is_empty());
    assert_eq!(fs::read(root.join("note.txt")).unwrap(), bytes);
    assert!(!root.join("child").exists());
    assert!(peek_registration_marker(&binding).unwrap().is_none());
    assert!(!root.join(".kio").join(JOURNAL_LEAF).exists());
}

#[test]
fn refuses_child_initialization_started_after_registration_planning() {
    let name = "root_registration::recovery_tests::refuses_child_initialization_started_after_registration_planning";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, _, _, _) = setup();
    let binding = ManagementBinding::bind(&root).unwrap();
    let nodes = plan_new(&root, binding.clone()).unwrap();
    let mut journal = Journal {
        version: 1,
        operation_id: new_ulid(&root),
        target_root: root.display().to_string(),
        phase: Phase::Prepared,
        nodes: nodes.iter().map(node_to_journal).collect(),
    };
    let bytes = encode(&journal).unwrap();
    let control = control_for(&binding).unwrap();
    control
        .create_directory_all(Path::new("child-initializations"))
        .unwrap();
    let pending = Path::new("child-initializations/01ARZ3NDEKTSV4RRFFQ69G5FAV.json");
    control
        .write_atomic(
            pending,
            b"concurrent pending initialization",
            Publication::CreateOnly,
        )
        .unwrap();
    let before = read_registration_recovery_record(&binding).unwrap();
    assert!(execute(&binding, &mut journal, bytes, false).is_err());
    assert_eq!(read_registration_recovery_record(&binding).unwrap(), before);
    assert!(!control.contains_entry(Path::new(JOURNAL_LEAF)).unwrap());
    assert!(peek_registration_marker(&binding).unwrap().is_none());
    assert_eq!(
        control.read_optional(pending, 128).unwrap(),
        Some(b"concurrent pending initialization".to_vec())
    );
}

#[test]
fn refuses_retirement_if_absent_child_reappears_before_resume() {
    let name = "root_registration::recovery_tests::refuses_retirement_if_absent_child_reappears_before_resume";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, _, _, _) = setup();
    fs::remove_dir_all(root.join("child")).unwrap();
    persist_cut(&root, Phase::Prepared, false);
    fs::create_dir(root.join("child")).unwrap();
    fs::write(root.join("child/new.txt"), b"new unregistered content").unwrap();
    let before = preserved_bytes(&root);
    let binding = ManagementBinding::bind(&root).unwrap();
    let control = control_for(&binding).unwrap();
    let bytes = control
        .read_optional(Path::new(JOURNAL_LEAF), MAX_JOURNAL_BYTES)
        .unwrap()
        .unwrap();
    let mut journal = decode(&bytes, &root).unwrap();
    assert!(execute(&binding, &mut journal, bytes.clone(), true).is_err());
    assert_eq!(preserved_bytes(&root), before);
    assert_eq!(
        control
            .read_optional(Path::new(JOURNAL_LEAF), MAX_JOURNAL_BYTES)
            .unwrap()
            .unwrap(),
        bytes
    );
    assert!(!root.join("child/.kio").exists());
}

#[test]
fn rejects_truncated_canonical_subtree_journal() {
    let name = "root_registration::recovery_tests::rejects_truncated_canonical_subtree_journal";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, _bytes, _root_id, _child_id) = setup();
    let binding = ManagementBinding::bind(&root).unwrap();
    let nodes = plan_new(&root, binding).unwrap();
    let journal = Journal {
        version: 1,
        operation_id: new_ulid(&root),
        target_root: root.display().to_string(),
        phase: Phase::Prepared,
        nodes: vec![node_to_journal(&nodes[0])],
    };
    let bytes = encode(&journal).unwrap();
    assert!(decode(&bytes, &root).is_err());
    let before = preserved_bytes(&root);
    let mut malformed = journal;
    assert!(execute(&nodes[0].binding, &mut malformed, bytes, false).is_err());
    assert_eq!(preserved_bytes(&root), before);
    assert!(!root.join(".kio").join(JOURNAL_LEAF).exists());
}

#[test]
fn recovers_revoked_partial_publication_cut() {
    let name = "root_registration::recovery_tests::recovers_revoked_partial_publication_cut";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, bytes, root_id, child_id) = setup();
    persist_cut(&root, Phase::Revoked, false);
    resume(&root);
    assert_recovered(&root, &bytes, &root_id, &child_id);
}

#[test]
fn recovers_published_registry_already_applied_cut() {
    let name = "root_registration::recovery_tests::recovers_published_registry_already_applied_cut";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, bytes, root_id, child_id) = setup();
    persist_cut(&root, Phase::Published, false);
    resume(&root);
    assert_recovered(&root, &bytes, &root_id, &child_id);
}

#[test]
fn recovers_finishing_child_marker_removed_cut() {
    let name = "root_registration::recovery_tests::recovers_finishing_child_marker_removed_cut";
    run_isolated(name);
    if std::env::var_os(GATE).is_none() {
        return;
    }
    let (_temp, root, bytes, root_id, child_id) = setup();
    persist_cut(&root, Phase::Finishing, true);
    resume(&root);
    assert_recovered(&root, &bytes, &root_id, &child_id);
}
