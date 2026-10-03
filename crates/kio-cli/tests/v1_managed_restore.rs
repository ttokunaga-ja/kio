mod support;

use support::canonical_tempdir;

use std::fs;
use std::process::Command;

use kio_core::scope::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn command(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .arg("--json")
        .args(args);
    command
}

fn ok(dir: &TempDir, args: &[&str]) -> Value {
    let output = command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let output = command(dir, args).output().unwrap();
    assert_eq!(output.status.code(), Some(code), "{args:?}");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn index(dir: &TempDir) -> String {
    ok(dir, &["index", "--offline"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn write_new_working_fixture(dir: &TempDir, leaf: &str, bytes: &[u8]) {
    #[cfg(windows)]
    {
        use kio_core::store_dir::{Publication, StoreDirectory};
        let target = StoreDirectory::open(dir.path()).unwrap();
        let owner = StoreDirectory::open(&dir.path().join(".kio")).unwrap();
        target
            .write_atomic_with_owner(
                &owner,
                std::path::Path::new(leaf),
                bytes,
                Publication::CreateOnly,
            )
            .unwrap();
    }
    #[cfg(not(windows))]
    fs::write(dir.path().join(leaf), bytes).unwrap();
}

fn fixture() -> (TempDir, String, String) {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    write_new_working_fixture(&dir, "a.md", b"old unique orchid phrase");
    write_new_working_fixture(&dir, "b.md", b"old b");
    let source = index(&dir);
    fs::write(dir.path().join("a.md"), b"new unique tulip phrase").unwrap();
    write_new_working_fixture(&dir, "c.md", b"keep c");
    let head = index(&dir);
    (dir, source, head)
}

#[test]
fn managed_restore_selected_path_is_a_linear_child_and_preserves_unselected_files() {
    let (dir, source, head) = fixture();
    fs::write(dir.path().join("c.md"), b"local dirty c").unwrap();
    let output = ok(&dir, &["restore", &source, "--path", "a.md"]);
    let restored = output["apply"]["commit_hash"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::read(dir.path().join("a.md")).unwrap(),
        b"old unique orchid phrase"
    );
    assert_eq!(fs::read(dir.path().join("c.md")).unwrap(), b"local dirty c");
    assert_eq!(output["source_commit"], source);
    assert_eq!(output["expected_head"], head);
    assert_eq!(output["projection_status"], "ready");

    let search = ok(&dir, &["search", "orchid", "--mode", "text"]);
    assert!(
        search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["evidence_pointer"]["path_at_commit"] == "a.md")
    );
    let dirty_search = ok(&dir, &["search", "localdirty", "--mode", "text"]);
    assert!(dirty_search["results"].as_array().unwrap().is_empty());

    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(&restored).unwrap();
    assert_eq!(commit.parent.as_deref(), Some(head.as_str()));
    assert_eq!(commit.restore_provenance.unwrap().source_commit, source);
}

#[test]
fn managed_restore_preview_and_expected_head_conflict_do_not_mutate() {
    let (dir, source, head) = fixture();
    let before = fs::read(dir.path().join("a.md")).unwrap();
    let preview = ok(&dir, &["restore", &source, "--preview"]);
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["expected_head"], head);
    assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), before);
    assert_eq!(
        fs::read_to_string(dir.path().join(".kio/HEAD"))
            .unwrap()
            .trim(),
        head
    );

    let error = failure(&dir, &["restore", &source, "--expected-head", &source], 3);
    assert_eq!(error["error_code"], "KIO-E-MANAGED-RESTORE-CONFLICT-001");
    assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), before);
}

#[test]
fn managed_restore_default_is_policy_filtered_and_explicit_denial_fails() {
    let (dir, source, _) = fixture();
    fs::write(dir.path().join(".kioignore"), b"b.md\n").unwrap();
    let preview = ok(&dir, &["restore", &source, "--preview"]);
    let paths = preview["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"a.md"));
    assert!(!paths.contains(&"b.md"));

    let error = failure(&dir, &["restore", &source, "--path", "b.md"], 2);
    assert_eq!(error["error_code"], "KIO-E-MANAGED-RESTORE-POLICY-001");
}
