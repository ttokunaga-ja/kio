mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore};
use kio_core::dag::{CommitObject, CommitStats, CommitType, build_tree};
use kio_core::portable::{PORTABLE_TAGS_DIRECTORY, portable_leaf_error, portable_tag_leaf};
use kio_core::scope::Repository;
use serde_json::Value;

const TEST_ENV: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_WINDOWS_PROFILE",
];

fn command(cwd: &Path, device: &Path) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in TEST_ENV {
        command.env_remove(name);
    }
    command
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", device.join("config"))
        .env("XDG_DATA_HOME", device.join("data"))
        .env("XDG_CACHE_HOME", device.join("cache"));
    command
}

fn json_success(cwd: &Path, device: &Path, args: &[&str]) -> Value {
    let output = command(cwd, device)
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(cwd: &Path, device: &Path, args: &[&str], code: i32) -> Value {
    let output = command(cwd, device)
        .arg("--json")
        .args(args)
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[cfg(windows)]
fn windows_no_home_command(cwd: &Path, profile: &Path) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in TEST_ENV {
        command.env_remove(name);
    }
    command
        .current_dir(cwd)
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env("KIO_TEST_WINDOWS_PROFILE", profile);
    command
}

#[cfg(windows)]
fn windows_no_home_json(cwd: &Path, profile: &Path, args: &[&str]) -> Value {
    let output = windows_no_home_command(cwd, profile)
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[cfg(windows)]
#[test]
fn home_and_xdg_unset_use_windows_profile_without_cwd_device_state() {
    let scope = tempfile::tempdir().unwrap();
    let profile = canonical_tempdir();
    windows_no_home_json(scope.path(), profile.path(), &["init"]);
    fs::write(
        scope.path().join("profile.md"),
        "# Profile\n\nwindows profile fallback marker\n",
    )
    .unwrap();
    windows_no_home_json(scope.path(), profile.path(), &["index"]);
    let search = windows_no_home_json(
        scope.path(),
        profile.path(),
        &["search", "windows profile fallback"],
    );
    let pointer = search["results"][0]["evidence_pointer"].to_string();
    fs::remove_file(scope.path().join("profile.md")).unwrap();
    let viewed = windows_no_home_json(scope.path(), profile.path(), &["view", &pointer]);

    assert!(
        profile
            .path()
            .join(".local/share/kio/scope-registry.sqlite")
            .is_file()
    );
    assert!(profile.path().join(".local/share/kio/cursor-key").is_file());
    assert!(Path::new(viewed["path"].as_str().unwrap()).starts_with(profile.path().join(".cache")));
    for relative in ["kio", ".config", ".local", ".cache"] {
        assert!(
            !scope.path().join(relative).exists(),
            "device state must not be created under CWD: {relative}"
        );
    }
}

#[test]
fn tag_names_use_portable_leaves_and_case_insensitive_collisions() {
    let scope = tempfile::tempdir().unwrap();
    let device = canonical_tempdir();
    json_success(scope.path(), device.path(), &["init"]);
    fs::write(scope.path().join("doc.md"), "first").unwrap();
    let first = json_success(
        scope.path(),
        device.path(),
        &["snapshot", "create", "-m", "first"],
    );

    for invalid in [
        "CON",
        "AUX.txt",
        "COM¹",
        "LPT³.txt",
        "question?",
        "stream:ads",
        "trailing.",
        "trailing ",
    ] {
        let error = json_failure(scope.path(), device.path(), &["tag", invalid], 2);
        assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001", "{invalid:?}");
    }

    let created = json_success(scope.path(), device.path(), &["tag", "Release"]);
    let physical = PathBuf::from(created["path"].as_str().unwrap());
    assert_eq!(
        physical,
        scope
            .path()
            .canonicalize()
            .unwrap()
            .join(".kio/refs")
            .join(PORTABLE_TAGS_DIRECTORY)
            .join(portable_tag_leaf("Release"))
    );
    let leaf = physical.file_name().unwrap().to_str().unwrap();
    assert_eq!(portable_leaf_error(leaf), None);
    assert_ne!(leaf, "Release");
    assert_eq!(
        fs::read_to_string(&physical).unwrap(),
        first["commit_hash"].as_str().unwrap()
    );

    let collision = json_failure(scope.path(), device.path(), &["tag", "release"], 2);
    assert_eq!(collision["error_code"], "KIO-E-COMMIT-TAG-001");
    json_success(scope.path(), device.path(), &["diff", "release", "HEAD"]);

    let error = json_failure(scope.path(), device.path(), &["tag", "head"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
    let error = json_failure(scope.path(), device.path(), &["diff", "head", "HEAD"], 2);
    assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");

    // A tag whose logical name is literally another tag's canonical leaf is
    // still its own tag: the leaf is `sha256` over the folded logical name, so
    // naming the digest cannot alias the tag that digest belongs to.
    fs::write(scope.path().join("doc.md"), "second").unwrap();
    let second = json_success(
        scope.path(),
        device.path(),
        &["snapshot", "create", "-m", "second"],
    );
    json_success(scope.path(), device.path(), &["tag", leaf]);
    let leaf_copy = json_success(scope.path(), device.path(), &["tag", "leaf-copy", leaf]);
    assert_eq!(leaf_copy["commit_hash"], second["commit_hash"]);
    let canonical_copy = json_success(
        scope.path(),
        device.path(),
        &["tag", "canonical-copy", "Release"],
    );
    assert_eq!(canonical_copy["commit_hash"], first["commit_hash"]);
}

#[test]
fn open_cache_derives_a_portable_leaf_from_hostile_logical_basename() {
    let scope = tempfile::tempdir().unwrap();
    let device = canonical_tempdir();
    json_success(scope.path(), device.path(), &["init"]);
    fs::write(
        scope.path().join("report.md"),
        "# Cache\n\nportable cache marker text\n",
    )
    .unwrap();
    json_success(scope.path(), device.path(), &["index"]);
    let search = json_success(
        scope.path(),
        device.path(),
        &["search", "portable cache marker"],
    );
    let mut pointer = search["results"][0]["evidence_pointer"].clone();
    let hostile = "CON?.PDF:stream. ";
    // A pointer's path is part of its authenticated commit/tree binding. Model
    // this Unix-originated historical logical basename in a child commit rather
    // than forging `path_at_commit` on the search result.
    let repo = Repository::open(scope.path()).unwrap();
    let parent_hash = repo.head_commit_hash().unwrap().unwrap();
    let parent = repo.read_commit(&parent_hash).unwrap();
    let mut tree = repo.read_tree(&parent.tree).unwrap();
    let raw_hash = pointer["raw_hash"].as_str().unwrap();
    tree.entries
        .iter_mut()
        .find(|entry| entry.raw_hash == raw_hash)
        .unwrap()
        .path = hostile.to_owned();
    let tree = build_tree(tree.entries).unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let (tree_hash, _) = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&tree).unwrap())
        .unwrap();
    let commit = CommitObject::new(
        tree_hash,
        Some(parent_hash),
        "2026-09-09T00:00:00Z".to_owned(),
        "fixture: hostile historical logical basename".to_owned(),
        parent.tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 1,
            files_deleted: 0,
        },
        CommitType::Manual,
    )
    .unwrap();
    let (commit_hash, _) = store
        .write_json(ObjectKind::Commit, &serde_json::to_value(&commit).unwrap())
        .unwrap();
    fs::write(repo.kio_dir().join("HEAD"), format!("{commit_hash}\n")).unwrap();
    pointer["commit"] = Value::String(commit_hash);
    pointer["path_at_commit"] = Value::String(hostile.to_owned());
    fs::remove_file(scope.path().join("report.md")).unwrap();

    let pointer_text = pointer.to_string();
    let viewed = json_success(scope.path(), device.path(), &["view", &pointer_text]);
    let cache = PathBuf::from(viewed["path"].as_str().unwrap());
    assert!(cache.starts_with(device.path().join("cache/kio/open")));
    let leaf = cache.file_name().unwrap().to_str().unwrap();
    assert_ne!(leaf, hostile);
    assert_eq!(portable_leaf_error(leaf), None, "{leaf:?}");
    assert!(cache.is_file());
}
