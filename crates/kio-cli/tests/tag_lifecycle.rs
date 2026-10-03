mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn kio(home: &Path, scope: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
        "KIO_FIXED_NOW",
    ] {
        command.env_remove(name);
    }
    command
        .current_dir(scope)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(args);
    command
}

fn json_success(home: &Path, scope: &Path, args: &[&str]) -> Value {
    let output = kio(home, scope, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(home: &Path, scope: &Path, args: &[&str], code: i32) -> Value {
    let output = kio(home, scope, args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn delete_tag_preserves_history_other_refs_and_name_audit_then_allows_recreation() {
    let scope = canonical_tempdir();
    let home = canonical_tempdir();
    json_success(home.path(), scope.path(), &["init"]);
    fs::write(scope.path().join("example.txt"), b"first version").unwrap();
    let first = json_success(home.path(), scope.path(), &["snapshot", "create"]);
    let first_hash = first["commit_hash"].as_str().unwrap();
    fs::write(scope.path().join("example.txt"), b"second version").unwrap();
    let second = json_success(home.path(), scope.path(), &["snapshot", "create"]);
    let head_hash = second["commit_hash"].as_str().unwrap();

    let created = json_success(home.path(), scope.path(), &["tag", "citation", first_hash]);
    let retained = json_success(home.path(), scope.path(), &["tag", "retained", head_hash]);
    assert_eq!(created["commit_hash"], first_hash);
    let tag_path = Path::new(created["path"].as_str().unwrap());
    let other_path = Path::new(retained["path"].as_str().unwrap());
    let names_path = tag_path.parent().unwrap().join("names.jsonl");
    let audit_before = fs::read(&names_path).unwrap();
    let other_ref_before = fs::read(other_path).unwrap();
    let first_object_before = json_success(home.path(), scope.path(), &["inspect", first_hash]);
    let head_object_before = json_success(home.path(), scope.path(), &["inspect", head_hash]);

    let deleted = json_success(home.path(), scope.path(), &["tag", "--delete", "citation"]);
    assert_eq!(deleted["operation"], "tag_delete");
    assert_eq!(deleted["status"], "deleted");
    assert_eq!(deleted["tag"], "citation");
    assert_eq!(deleted["commit_hash"], first_hash);
    assert_eq!(deleted["path"], created["path"]);
    assert!(!tag_path.exists());
    assert_eq!(fs::read(&names_path).unwrap(), audit_before);
    assert_eq!(fs::read(other_path).unwrap(), other_ref_before);
    assert_eq!(
        json_success(home.path(), scope.path(), &["inspect", first_hash]),
        first_object_before
    );
    assert_eq!(
        json_success(home.path(), scope.path(), &["inspect", head_hash]),
        head_object_before
    );
    let log = json_success(home.path(), scope.path(), &["log"]);
    assert_eq!(log["commits"][0]["commit_hash"], head_hash);

    let recreated = json_success(home.path(), scope.path(), &["tag", "citation", first_hash]);
    assert_eq!(recreated["path"], created["path"]);
    assert_eq!(fs::read_to_string(tag_path).unwrap(), first_hash);
    assert!(fs::read(&names_path).unwrap().starts_with(&audit_before));
}

#[test]
fn missing_invalid_and_commit_conflict_fail_without_deleting_another_tag() {
    let scope = canonical_tempdir();
    let home = canonical_tempdir();
    json_success(home.path(), scope.path(), &["init"]);
    fs::write(scope.path().join("example.txt"), b"version").unwrap();
    let snapshot = json_success(home.path(), scope.path(), &["snapshot", "create"]);
    let hash = snapshot["commit_hash"].as_str().unwrap();
    let created = json_success(home.path(), scope.path(), &["tag", "safe", hash]);
    let safe_path = Path::new(created["path"].as_str().unwrap());
    let safe_before = fs::read(safe_path).unwrap();

    let missing = json_failure(
        home.path(),
        scope.path(),
        &["tag", "--delete", "missing"],
        4,
    );
    assert_eq!(missing["error_code"], "KIO-E-STORE-NOT-FOUND-001");
    for invalid in ["HEAD", "..", "a/b"] {
        let error = json_failure(home.path(), scope.path(), &["tag", "--delete", invalid], 2);
        assert!(error["error_code"].as_str().is_some());
    }
    let conflict = json_failure(
        home.path(),
        scope.path(),
        &["tag", "--delete", "safe", hash],
        2,
    );
    assert_eq!(conflict["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert_eq!(fs::read(safe_path).unwrap(), safe_before);
}
