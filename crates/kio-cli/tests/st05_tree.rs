//! ST05 directory size is advisory; a snapshot must preserve every input.
mod support;

use assert_cmd::Command;
use kio_core::scope::Repository;
use std::fs;

#[test]
fn snapshot_warns_and_saves_every_file_above_ten_thousand() {
    let dir = support::canonical_tempdir();
    let scope = dir.path().join("scope");
    fs::create_dir(&scope).unwrap();
    let repo = Repository::init(&scope).unwrap();
    for index in 0..10_001 {
        fs::write(scope.join(format!("{index:05}.txt")), b"").unwrap();
    }
    let output = Command::cargo_bin("kio")
        .unwrap()
        .current_dir(&scope)
        .envs(support::home_env(dir.path()))
        .args(["snapshot", "create", "-m", "large tree", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("10,000"), "{stderr}");
    assert!(
        stderr.contains("subfolders") && stderr.contains("ignore"),
        "{stderr}"
    );
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    assert_eq!(repo.read_tree(&commit.tree).unwrap().entries.len(), 10_001);
    let inspected = Command::cargo_bin("kio")
        .unwrap()
        .current_dir(&scope)
        .envs(support::home_env(dir.path()))
        .args(["inspect", &commit.tree, "--json"])
        .output()
        .unwrap();
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let tree: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(tree["object_type"], "tree");
    assert_eq!(tree["entries"].as_array().unwrap().len(), 10_001);
}
