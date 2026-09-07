use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

#[derive(Debug, PartialEq, Eq)]
enum TreeEntry {
    Directory,
    File(Vec<u8>),
    Symlink(Vec<u8>),
}

fn kio(home: &Path) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command
        .env_remove("KIO_FIXED_NOW")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"));
    command
}

fn file_tree(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn collect(root: &Path, directory: &Path, tree: &mut BTreeMap<PathBuf, TreeEntry>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                tree.insert(relative, TreeEntry::Directory);
                collect(root, &path, tree);
            } else if kind.is_file() {
                tree.insert(relative, TreeEntry::File(fs::read(path).unwrap()));
            } else if kind.is_symlink() {
                tree.insert(
                    relative,
                    TreeEntry::Symlink(
                        fs::read_link(path)
                            .unwrap()
                            .into_os_string()
                            .into_encoded_bytes(),
                    ),
                );
            }
        }
    }

    let mut tree = BTreeMap::new();
    collect(root, root, &mut tree);
    tree
}

#[test]
fn index_preview_leaves_the_repository_file_tree_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    kio(home.path())
        .arg("init")
        .current_dir(repo.path())
        .assert()
        .success();
    fs::write(repo.path().join("document.txt"), "preview input").unwrap();
    let before = file_tree(repo.path());

    kio(home.path())
        .args(["index", "--preview", "--offline"])
        .current_dir(repo.path())
        .assert()
        .success();

    assert_eq!(file_tree(repo.path()), before);
}

#[test]
fn search_all_scopes_rejects_scope_selectors_at_parse_time() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        ["search", "needle", "--all-scopes", "--scope", "."].as_slice(),
        ["search", "needle", "--all-scopes", "--descendants"].as_slice(),
    ] {
        let output = kio(home.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "args={args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
