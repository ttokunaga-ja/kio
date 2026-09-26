//! Phase 4 milestone 1: the public GC surface is a deterministic read-only plan.

mod support;

use support::canonical_tempdir;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

fn kio(home: &Path) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in [
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "KIO_TEST_GEMINI_EMBED",
        "KIO_TEST_MISTRAL_OCR",
    ] {
        command.env_remove(name);
    }
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("KIO_FIXED_NOW", "2026-08-14T00:00:00Z");
    command
}

#[derive(Debug, PartialEq, Eq)]
struct StoreImage {
    directories: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

fn store_image(root: &Path) -> StoreImage {
    fn walk(
        root: &Path,
        at: &Path,
        directories: &mut BTreeSet<PathBuf>,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                directories.insert(relative);
                walk(root, &path, directories, files);
            } else {
                assert!(metadata.is_file());
                files.insert(relative, fs::read(path).unwrap());
            }
        }
    }

    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    walk(root, root, &mut directories, &mut files);
    StoreImage { directories, files }
}

#[test]
fn gc_dry_run_is_deterministic_and_does_not_change_the_store() {
    let scope = tempfile::tempdir().unwrap();
    let home = canonical_tempdir();
    kio(home.path())
        .arg("init")
        .current_dir(scope.path())
        .assert()
        .success();

    let kio_dir = scope.path().join(".kio");
    let before = store_image(&kio_dir);
    assert!(!kio_dir.join("gc").exists());

    let first = kio(home.path())
        .args(["gc", "--dry-run", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = kio(home.path())
        .args(["gc", "--dry-run", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, second.stdout);

    let value: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(value["status"], "dry_run");
    assert_eq!(value["candidate_count"], 0);
    assert_eq!(value["candidate_tree_count"], 0);
    assert_eq!(value["estimated_bytes"], 0);
    assert_eq!(value["object_kinds_planned"], serde_json::json!(["tree"]));

    let first_human = kio(home.path())
        .args(["gc", "--dry-run"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(first_human.status.success());
    let second_human = kio(home.path())
        .args(["gc", "--dry-run"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(second_human.status.success());
    assert_eq!(first_human.stdout, second_human.stdout);
    let human_value: Value = serde_json::from_slice(&first_human.stdout).unwrap();
    assert_eq!(human_value, value);

    assert_eq!(store_image(&kio_dir), before);
    assert!(!kio_dir.join("gc").exists());
}

#[test]
fn gc_json_execution_requires_yes_even_when_the_plan_is_empty() {
    let scope = tempfile::tempdir().unwrap();
    let home = canonical_tempdir();
    kio(home.path())
        .arg("init")
        .current_dir(scope.path())
        .assert()
        .success();
    let kio_dir = scope.path().join(".kio");
    let before = store_image(&kio_dir);

    let output = kio(home.path())
        .args(["gc", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert!(error["message"].as_str().unwrap().contains("--yes"));
    assert_eq!(store_image(&kio_dir), before);

    let authorized = kio(home.path())
        .args(["gc", "--yes", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(authorized.status.success());
    let report: Value = serde_json::from_slice(&authorized.stdout).unwrap();
    assert_eq!(report["status"], "dry_run");
    assert_eq!(report["candidate_count"], 0);
}

#[test]
fn gc_dry_run_and_yes_is_invalid_usage() {
    let scope = tempfile::tempdir().unwrap();
    let home = canonical_tempdir();
    kio(home.path())
        .arg("init")
        .current_dir(scope.path())
        .assert()
        .success();

    let output = kio(home.path())
        .args(["gc", "--dry-run", "--yes", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn gc_outside_a_scope_is_invalid_usage_without_creating_a_store() {
    let directory = tempfile::tempdir().unwrap();
    let home = canonical_tempdir();
    let before = fs::read_dir(directory.path()).unwrap().count();

    let output = kio(home.path())
        .args(["gc", "--dry-run", "--json"])
        .current_dir(directory.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), before);
    assert!(!directory.path().join(".kio").exists());
}
