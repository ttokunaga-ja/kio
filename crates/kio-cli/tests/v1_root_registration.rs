//! End-to-end contracts for explicit recovery of a moved managed root.

mod support;

use support::canonical_tempdir;

use assert_cmd::Command;
use kio_index::registry::RegistryDb;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    device: PathBuf,
    root_parent: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = canonical_tempdir();
        let device = temp.path().join("device");
        let root_parent = temp.path().join("managed");
        for path in std::iter::once(temp.path().to_path_buf())
            .chain([device.clone(), root_parent.clone()])
            .chain(["home", "data", "config", "cache", "tmp"].map(|leaf| device.join(leaf)))
        {
            fs::create_dir_all(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        Self {
            _temp: temp,
            device,
            root_parent,
        }
    }
    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("kio").unwrap();
        command.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command
            .current_dir(cwd)
            .env("HOME", self.device.join("home"))
            .env("USERPROFILE", self.device.join("home"))
            .env("XDG_DATA_HOME", self.device.join("data"))
            .env("XDG_CONFIG_HOME", self.device.join("config"))
            .env("XDG_CACHE_HOME", self.device.join("cache"))
            .env("TMPDIR", self.device.join("tmp"))
            .env("TEMP", self.device.join("tmp"))
            .env("TMP", self.device.join("tmp"))
            .env("KIO_TEST_GEMINI_EMBED", "mock")
            .args(args);
        command
    }
    fn json(&self, cwd: &Path, args: &[&str]) -> Value {
        let bytes = self
            .command(cwd, args)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&bytes).unwrap()
    }
    fn run(&self, cwd: &Path, args: &[&str]) -> (i32, Value) {
        let output = self.command(cwd, args).arg("--json").output().unwrap();
        let bytes = if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        (
            output.status.code().unwrap(),
            serde_json::from_slice(bytes).unwrap(),
        )
    }
    fn registry(&self) -> RegistryDb {
        RegistryDb::open_read_only(self.device.join("data/kio/scope-registry.sqlite")).unwrap()
    }
}

fn management(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join(".kio/management.json")).unwrap()).unwrap()
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn byte_fingerprint(root: &Path) -> Vec<(PathBuf, String)> {
    fn visit(base: &Path, here: &Path, out: &mut Vec<(PathBuf, String)>) {
        assert!(here.strip_prefix(base).unwrap().components().count() < 64);
        for entry in fs::read_dir(here).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(base, &entry.path(), out);
            } else {
                assert!(entry.file_type().unwrap().is_file());
                assert!(entry.metadata().unwrap().len() <= 64 * 1024 * 1024);
                assert!(out.len() < 10_000);
                out.push((
                    entry.path().strip_prefix(base).unwrap().to_path_buf(),
                    Sha256::digest(fs::read(entry.path()).unwrap())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn moved_root_and_enrolled_child_rebind_without_changing_repository_content() {
    let f = Fixture::new();
    let old = f.root_parent.join("old");
    let child = old.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::write(old.join("note.txt"), b"durable knowledge\n").unwrap();
    f.json(&old, &["init"]);
    f.json(&old, &["index", "--offline"]);
    let root_before = management(&old);
    let child_before = management(&child);
    let head = fs::read(old.join(".kio/HEAD")).unwrap();
    let config = fs::read(old.join(".kio/config.toml")).unwrap();
    let knowledge = byte_fingerprint(&old)
        .into_iter()
        .filter(|(path, _)| {
            !path.ends_with(".kio/management.json") && !path.ends_with(".kio/.lock")
        })
        .collect::<Vec<_>>();
    let moved = f.root_parent.join("moved");
    fs::rename(&old, &moved).unwrap();

    assert_ne!(f.run(&moved, &["index", "--offline"]).0, 0);
    let before_preview = byte_fingerprint(&moved);
    let preview = f.json(&moved, &["root", "register", "--preview"]);
    assert_eq!(preview["preview"], true);
    assert!(!moved.join(".kio/root-registration.json").exists());
    assert_eq!(byte_fingerprint(&moved), before_preview);
    f.json(&moved, &["root", "register", "--yes"]);

    let root_after = management(&moved);
    let child_after = management(&moved.join("child"));
    assert_eq!(root_after["scope_id"], root_before["scope_id"]);
    assert_eq!(child_after["scope_id"], child_before["scope_id"]);
    assert_eq!(
        root_after["registration_generation"].as_u64(),
        root_before["registration_generation"]
            .as_u64()
            .map(|n| n + 1)
    );
    assert_eq!(
        child_after["registration_generation"].as_u64(),
        child_before["registration_generation"]
            .as_u64()
            .map(|n| n + 1)
    );
    assert_eq!(fs::read(moved.join(".kio/HEAD")).unwrap(), head);
    assert_eq!(fs::read(moved.join(".kio/config.toml")).unwrap(), config);
    assert_eq!(
        fs::read(moved.join("note.txt")).unwrap(),
        b"durable knowledge\n"
    );
    assert_eq!(
        byte_fingerprint(&moved)
            .into_iter()
            .filter(|(path, _)| !path.ends_with(".kio/management.json")
                && !path.ends_with(".kio/.lock"))
            .collect::<Vec<_>>(),
        knowledge
    );
    let rows = f
        .registry()
        .lookup_scope_id(root_after["scope_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].indexed);
    f.json(&moved, &["index", "--offline"]);
}

#[test]
fn copied_tree_is_refused_while_original_registration_is_live() {
    let f = Fixture::new();
    let source = f.root_parent.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"a").unwrap();
    f.json(&source, &["init"]);
    f.json(&source, &["index", "--offline"]);
    let copy = f.root_parent.join("copy");
    copy_tree(&source, &copy);
    let before = fs::read(copy.join(".kio/management.json")).unwrap();
    assert_ne!(f.run(&copy, &["root", "register", "--yes"]).0, 0);
    assert_eq!(fs::read(copy.join(".kio/management.json")).unwrap(), before);
}

#[test]
fn former_child_with_grandchild_can_be_explicitly_detached_after_move() {
    let f = Fixture::new();
    let parent = f.root_parent.join("parent");
    let child = parent.join("child");
    let grandchild = child.join("grandchild");
    fs::create_dir_all(&grandchild).unwrap();
    fs::write(grandchild.join("nested.txt"), b"nested history").unwrap();
    f.json(&parent, &["init"]);
    f.json(&parent, &["index", "--offline"]);
    let child_before = management(&child);
    let grandchild_before = management(&grandchild);
    let detached = f.root_parent.join("detached");
    fs::rename(&child, &detached).unwrap();

    let preview = f.json(&detached, &["root", "register", "--preview"]);
    assert_eq!(preview["detaches_from_former_parent"], true);
    f.json(&detached, &["root", "register", "--yes"]);
    let child_after = management(&detached);
    let grandchild_after = management(&detached.join("grandchild"));
    assert_eq!(child_after["scope_id"], child_before["scope_id"]);
    assert_eq!(child_after["authority"]["kind"], "root");
    assert_eq!(grandchild_after["scope_id"], grandchild_before["scope_id"]);
    assert_eq!(
        grandchild_after["authority"]["root_scope_id"],
        child_after["scope_id"]
    );
    assert_eq!(
        fs::read(detached.join("grandchild/nested.txt")).unwrap(),
        b"nested history"
    );
}

#[test]
fn move_away_and_back_advances_the_registration_generation_each_time() {
    let f = Fixture::new();
    let original = f.root_parent.join("original");
    fs::create_dir(&original).unwrap();
    fs::write(original.join("knowledge.txt"), b"preserve me").unwrap();
    f.json(&original, &["init"]);
    f.json(&original, &["index", "--offline"]);
    let generation = management(&original)["registration_generation"]
        .as_u64()
        .unwrap();
    let away = f.root_parent.join("away");
    fs::rename(&original, &away).unwrap();
    f.json(&away, &["root", "register", "--yes"]);
    assert_eq!(
        management(&away)["registration_generation"].as_u64(),
        Some(generation + 1)
    );
    fs::rename(&away, &original).unwrap();
    f.json(&original, &["root", "register", "--yes"]);
    let after = management(&original);
    assert_eq!(
        after["registration_generation"].as_u64(),
        Some(generation + 2)
    );
    assert_eq!(
        fs::read(original.join("knowledge.txt")).unwrap(),
        b"preserve me"
    );
}

#[test]
fn move_registration_revokes_existing_mock_embedding_grants_and_never_reactivates_them() {
    let f = Fixture::new();
    let original = f.root_parent.join("granted");
    fs::create_dir(&original).unwrap();
    fs::write(original.join("note.md"), b"grant fixture").unwrap();
    f.json(&original, &["init"]);
    f.json(
        &original,
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    );
    let status = f.json(&original, &["adapter", "status", "gemini_embedding_2"]);
    let ids = status["grants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["state"] == "active")
        .map(|g| g["grant_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        !ids.is_empty(),
        "mock approval must create active grants: {status}"
    );
    let away = f.root_parent.join("granted-away");
    fs::rename(&original, &away).unwrap();
    f.json(&away, &["root", "register", "--yes"]);
    let moved_status = f.json(&away, &["adapter", "status", "gemini_embedding_2"]);
    for id in &ids {
        assert!(
            moved_status["grants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|grant| grant["grant_id"] == *id && grant["state"] == "revoked")
        );
    }
    fs::rename(&away, &original).unwrap();
    f.json(&original, &["root", "register", "--yes"]);
    let after = f.json(&original, &["adapter", "status", "gemini_embedding_2"]);
    for id in ids {
        assert!(
            after["grants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["grant_id"] == id && g["state"] == "revoked"),
            "grant {id} reactivated or disappeared: {after}"
        );
    }
}

#[test]
fn copied_enrolled_child_is_refused_even_when_only_its_root_is_in_the_device_registry() {
    let f = Fixture::new();
    let parent = f.root_parent.join("parent-copy-guard");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("source.txt"), b"original child").unwrap();
    f.json(&parent, &["init"]);
    f.json(&parent, &["index", "--offline"]);
    let copied = f.root_parent.join("copied-child");
    // A derived cache row can disappear independently of child authority.
    let registry = RegistryDb::open(f.device.join("data/kio/scope-registry.sqlite")).unwrap();
    registry
        .remove(
            management(&child)["scope_id"].as_str().unwrap(),
            &child
                .canonicalize()
                .unwrap()
                .join(".kio")
                .display()
                .to_string(),
        )
        .unwrap();
    drop(registry);
    copy_tree(&child, &copied);
    let original_before = byte_fingerprint(&child);
    let copy_before = byte_fingerprint(&copied);
    let rows = f
        .registry()
        .lookup_scope_id(management(&parent)["scope_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "fixture must keep only the parent registration"
    );
    assert!(
        f.registry()
            .lookup_scope_id(management(&child)["scope_id"].as_str().unwrap())
            .unwrap()
            .is_empty()
    );
    assert_ne!(f.run(&copied, &["root", "register", "--yes"]).0, 0);
    assert_eq!(byte_fingerprint(&child), original_before);
    assert_eq!(byte_fingerprint(&copied), copy_before);
}

#[test]
fn imported_moved_subtree_preview_is_readonly_on_an_empty_second_device() {
    let source_device = Fixture::new();
    let old = source_device.root_parent.join("old-import");
    let child = old.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("imported.txt"), b"portable knowledge").unwrap();
    source_device.json(&old, &["init"]);
    source_device.json(&old, &["index", "--offline"]);
    let destination_device = Fixture::new();
    let imported = destination_device.root_parent.join("imported");
    fs::rename(&old, &imported).unwrap();
    assert!(
        !destination_device
            .device
            .join("data/kio/scope-registry.sqlite")
            .exists()
    );
    assert!(
        !destination_device
            .device
            .join("data/kio/grants/grants.json")
            .exists()
    );
    let before = byte_fingerprint(&imported);
    let preview = destination_device.json(&imported, &["root", "register", "--preview"]);
    assert_eq!(preview["preview"], true);
    assert_eq!(byte_fingerprint(&imported), before);
    assert!(
        !destination_device
            .device
            .join("data/kio/scope-registry.sqlite")
            .exists()
    );
    assert!(
        !destination_device
            .device
            .join("data/kio/grants/grants.json")
            .exists()
    );
    assert!(!destination_device.device.join("data/kio").exists());
    destination_device.json(&imported, &["root", "register", "--yes"]);
    assert!(
        destination_device
            .device
            .join("data/kio/scope-registry.sqlite")
            .exists()
    );
}

#[test]
fn foreign_platform_subtree_uses_recorded_path_syntax_only_for_explicit_registration() {
    let f = Fixture::new();
    let imported = f.root_parent.join("foreign-import");
    fs::create_dir_all(imported.join("child")).unwrap();
    fs::write(
        imported.join("note.txt"),
        b"portable across operating systems",
    )
    .unwrap();
    f.json(&imported, &["init"]);
    f.json(&imported, &["index", "--offline"]);
    for (index, relative) in ["", "child"].iter().enumerate() {
        let directory = imported.join(relative);
        let mut record = management(&directory);
        #[cfg(unix)]
        {
            record["canonical_root"] = Value::String(if relative.is_empty() {
                r"C:\Kio\Imported".into()
            } else {
                r"C:\Kio\Imported\child".into()
            });
            record["directory_identity"] =
                serde_json::to_value(kio_core::management::DirectoryIdentity::Windows {
                    volume_serial_number: 42,
                    file_index: index as u64 + 100,
                })
                .unwrap();
        }
        #[cfg(windows)]
        {
            record["canonical_root"] = Value::String(if relative.is_empty() {
                "/foreign/kio/imported".into()
            } else {
                "/foreign/kio/imported/child".into()
            });
            record["directory_identity"] =
                serde_json::to_value(kio_core::management::DirectoryIdentity::Unix {
                    device: 42,
                    inode: index as u64 + 100,
                })
                .unwrap();
        }
        fs::write(
            directory.join(".kio/management.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
    let mut parent = management(&imported);
    parent["children"]["child"]["directory_identity"] =
        management(&imported.join("child"))["directory_identity"].clone();
    fs::write(
        imported.join(".kio/management.json"),
        serde_json::to_vec(&parent).unwrap(),
    )
    .unwrap();
    assert_ne!(f.run(&imported, &["index", "--offline"]).0, 0);
    let before = byte_fingerprint(&imported);
    f.json(&imported, &["root", "register", "--preview"]);
    assert_eq!(byte_fingerprint(&imported), before);
    let result = f.json(&imported, &["root", "register", "--yes"]);
    assert_eq!(result["scopes_registered"], 2);
    f.json(&imported, &["index", "--offline"]);
}

#[test]
fn moved_root_registers_after_an_enrolled_child_was_deleted() {
    let f = Fixture::new();
    let original = f.root_parent.join("before-child-deletion");
    let child = original.join("deleted-child");
    fs::create_dir_all(&child).unwrap();
    fs::write(original.join("kept.txt"), b"retained root knowledge").unwrap();
    f.json(&original, &["init"]);
    f.json(&original, &["index", "--offline"]);
    let old_child = management(&child);
    fs::remove_dir_all(&child).unwrap();
    let moved = f.root_parent.join("after-child-deletion");
    fs::rename(&original, &moved).unwrap();
    let before = byte_fingerprint(&moved);
    let preview = f.json(&moved, &["root", "register", "--preview"]);
    assert_eq!(preview["child_memberships_retired"], 1);
    assert_eq!(byte_fingerprint(&moved), before);
    let result = f.json(&moved, &["root", "register", "--yes"]);
    assert_eq!(result["scopes_registered"], 1);
    assert_eq!(result["child_memberships_retired"], 1);
    assert!(
        management(&moved)["children"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert!(
        f.registry()
            .lookup_scope_id(old_child["scope_id"].as_str().unwrap())
            .unwrap()
            .is_empty()
    );
    assert!(!moved.join("deleted-child").exists());
    f.json(&moved, &["index", "--offline"]);
    assert_eq!(
        fs::read(moved.join("kept.txt")).unwrap(),
        b"retained root knowledge"
    );
}
