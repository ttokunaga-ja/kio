//! Milestone 8's public read-only inventory entry point.

#![cfg(any(unix, windows))]

mod support;

use support::canonical_tempdir;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn kio(home: &Path) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"));
    command
}

fn image(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                out.insert(
                    format!("{}/", path.strip_prefix(root).unwrap().display()),
                    Vec::new(),
                );
                walk(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().display().to_string(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn inventory_is_deterministic_read_only_and_prune_requires_dry_run() {
    let scope = tempfile::tempdir().unwrap();
    let home = canonical_tempdir();
    kio(home.path())
        .arg("init")
        .current_dir(scope.path())
        .assert()
        .success();
    let before = image(scope.path());
    let first = kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert_eq!(first.stdout, second.stdout);
    let value: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(value["operation"], "unreachable_object_inventory");
    assert_eq!(value["status"], "dry_run");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["read_only"], true);
    assert_eq!(value["diagnostic_only"], true);
    assert_eq!(value["mutation_authority"], false);
    let objects = value["objects"].as_array().unwrap();
    assert!(objects.windows(2).all(|pair| {
        let left = (
            pair[0]["kind"].as_str().unwrap(),
            pair[0]["hash"].as_str().unwrap(),
        );
        let right = (
            pair[1]["kind"].as_str().unwrap(),
            pair[1]["hash"].as_str().unwrap(),
        );
        left <= right
    }));
    for object in objects {
        assert!(object["hash"].as_str().unwrap().starts_with("sha256:"));
        assert!(matches!(
            object["classification"].as_str(),
            Some("candidate" | "protected" | "inventory_only")
        ));
        assert!(object["reason"].is_string());
        assert!(object["physical_bytes"].is_u64());
    }
    assert_eq!(before, image(scope.path()));
    kio(home.path())
        .args(["gc", "--prune-unreachable"])
        .current_dir(scope.path())
        .assert()
        .code(2);
    kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable", "--yes"])
        .current_dir(scope.path())
        .assert()
        .code(2);
    kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable", "--unknown"])
        .current_dir(scope.path())
        .assert()
        .code(2);
}

#[test]
fn inventory_human_output_is_deterministic_and_failures_do_not_write_stdout() {
    let scope = tempfile::tempdir().unwrap();
    let home = canonical_tempdir();
    kio(home.path())
        .arg("init")
        .current_dir(scope.path())
        .assert()
        .success();
    let before = image(scope.path());
    let first = kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    let second = kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(first.status.success());
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(before, image(scope.path()));
    fs::write(scope.path().join(".kio").join(".lock"), b"writer").unwrap();
    let before_failure = image(scope.path());
    let failure = kio(home.path())
        .args(["gc", "--dry-run", "--prune-unreachable", "--json"])
        .current_dir(scope.path())
        .output()
        .unwrap();
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert_eq!(before_failure, image(scope.path()));
}

#[cfg(windows)]
#[test]
fn inventory_missing_or_malformed_store_gate_fails_without_creating_or_repairing_it() {
    for state in ["missing", "nonempty", "directory"] {
        let scope = canonical_tempdir();
        let home = canonical_tempdir();
        kio(home.path())
            .arg("init")
            .current_dir(scope.path())
            .assert()
            .success();
        let gate = scope.path().join(".kio/.store-gate");
        assert_eq!(
            fs::read(&gate).unwrap(),
            b"",
            "init must publish the empty gate"
        );
        match state {
            "missing" => fs::remove_file(&gate).unwrap(),
            "nonempty" => fs::write(&gate, b"malformed gate\n").unwrap(),
            "directory" => {
                fs::remove_file(&gate).unwrap();
                fs::create_dir(&gate).unwrap();
            }
            _ => unreachable!(),
        }
        let before = image(scope.path());
        for _ in 0..2 {
            let failure = kio(home.path())
                .args(["gc", "--dry-run", "--prune-unreachable", "--json"])
                .current_dir(scope.path())
                .output()
                .unwrap();
            assert_eq!(
                failure.status.code(),
                Some(3),
                "{state}: {}",
                String::from_utf8_lossy(&failure.stderr)
            );
            assert!(
                failure.stdout.is_empty(),
                "{state}: failed inventory emitted results"
            );
            let error: Value = serde_json::from_slice(&failure.stderr).unwrap();
            assert_eq!(error["error_code"], "KIO-E-STORE-LOCKED-001", "{state}");
            assert_eq!(
                before,
                image(scope.path()),
                "{state}: read-only inventory changed scope"
            );
            assert!(
                !scope.path().join(".kio/.lock").exists(),
                "{state}: read-only inventory created a writer lock"
            );
        }
    }
}
