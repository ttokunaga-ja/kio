use super::*;
use std::{
    cell::RefCell,
    os::unix::fs::{PermissionsExt, symlink},
};
fn tree() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = fs::canonicalize(temp.path())
        .unwrap()
        .join("MacOSX26.5.sdk");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), "sdk").unwrap();
    (temp, root)
}
#[test]
fn metadata_requires_every_pinned_field() {
    let settings = serde_json::json!({"CanonicalName":"macosx26.5","Version":"26.5"});
    let system = serde_json::json!({"ProductName":"macOS","ProductVersion":"26.5","ProductBuildVersion":"25F70"});
    metadata(&settings, &system).unwrap();
    for (which, document) in [&settings, &system].into_iter().enumerate() {
        for key in document.as_object().unwrap().keys() {
            let mut changed = document.clone();
            changed[key] = Value::String("wrong".into());
            assert!(
                if which == 0 {
                    metadata(&changed, &system)
                } else {
                    metadata(&settings, &changed)
                }
                .is_err()
            );
        }
    }
    assert!(metadata(&Value::Null, &system).is_err());
}
#[test]
fn discovery_requires_pinned_version_and_build() {
    assert!(discovery_identity("26.6", "25F70").is_err());
    assert!(discovery_identity("26.5", "wrong").is_err());
}
#[test]
fn paths_reject_other_installations_and_malformed_paths() {
    for text in [
        "relative",
        "/Applications/Xcode_26.6.app/MacOSX.sdk",
        "/tmp/Other.sdk",
        "/tmp/MacOSX.sdk\n",
    ] {
        assert!(sdk_path(text, Path::new(SDKS)).is_err());
    }
}
#[test]
fn canonical_path_cannot_escape() {
    let (_temp, root) = tree();
    let parent = root.parent().unwrap();
    assert_eq!(sdk_path(root.to_str().unwrap(), parent).unwrap(), root);
    let alias = parent.join("MacOSX.sdk");
    symlink(root.file_name().unwrap(), &alias).unwrap();
    assert_eq!(sdk_path(alias.to_str().unwrap(), parent).unwrap(), root);
    fs::remove_file(&alias).unwrap();
    symlink("/tmp", &alias).unwrap();
    assert!(sdk_path(alias.to_str().unwrap(), parent).is_err());
}
#[test]
fn plan_preserves_internal_framework_links() {
    let (_temp, root) = tree();
    let framework = root.join("Framework.framework");
    fs::create_dir_all(framework.join("Versions/A")).unwrap();
    fs::write(framework.join("Versions/A/header"), "header").unwrap();
    symlink("A", framework.join("Versions/Current")).unwrap();
    symlink("Versions/Current/header", framework.join("header")).unwrap();
    assert_eq!(
        plan_tree(&root).unwrap()[&framework.join("header")].kind,
        Kind::Link("Versions/Current/header".into())
    );
}
#[test]
fn plan_rejects_external_dangling_cyclic_and_transient_escape_links() {
    let (_temp, root) = tree();
    for target in [
        "../outside",
        "/tmp",
        "missing",
        "link",
        "../MacOSX26.5.sdk/file",
        "file\n",
    ] {
        let link = root.join("link");
        symlink(target, &link).unwrap();
        assert!(plan_tree(&root).is_err(), "{target}");
        fs::remove_file(link).unwrap();
    }
}
#[test]
fn plan_rejects_special_entries_and_hardlinks() {
    let (_temp, root) = tree();
    let fifo = root.join("fifo");
    assert_eq!(
        unsafe { libc::mkfifo(cstring(&fifo).unwrap().as_ptr(), 0o600) },
        0
    );
    assert!(plan_tree(&root).unwrap_err().contains("unsupported"));
    fs::remove_file(fifo).unwrap();
    fs::hard_link(root.join("file"), root.join("hardlink")).unwrap();
    assert!(plan_tree(&root).unwrap_err().contains("hardlinked"));
}
#[test]
fn plan_rejects_special_permission_bits() {
    let (_temp, root) = tree();
    fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o4755)).unwrap();
    assert!(plan_tree(&root).unwrap_err().contains("special permission"));
}
#[test]
fn open_node_rejects_symlink_in_any_component() {
    let (_temp, root) = tree();
    symlink(".", root.join("link")).unwrap();
    assert!(open_node(&root.join("link/file")).is_err());
}
#[test]
fn metadata_reader_rejects_symlink_and_oversize() {
    let (_temp, root) = tree();
    let path = root.join("SDKSettings.json");
    symlink("file", &path).unwrap();
    assert!(bounded_metadata(&path).is_err());
    fs::remove_file(&path).unwrap();
    fs::write(&path, vec![b'a'; LIMIT as usize + 1]).unwrap();
    assert!(bounded_metadata(&path).is_err());
    assert!(bounded_metadata(&root).is_err());
}
#[test]
fn applications_exception_is_narrow() {
    assert!(trusted(Path::new("/Applications"), 0, 80, 0o775));
    for (path, uid, gid, mode) in [
        ("/other", 0, 80, 0o775),
        ("/Applications", 0, 0, 0o775),
        ("/other", 501, 0, 0o755),
        ("/other", 0, 0, 0o777),
    ] {
        assert!(!trusted(Path::new(path), uid, gid, mode));
    }
}
#[test]
fn command_environment_drops_ambient_overrides() {
    for developer in [true, false] {
        let cmd = command_spec("/usr/bin/xcrun", &[], developer);
        let vars: BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap(), v.unwrap().to_str().unwrap()))
            .collect();
        assert_eq!(vars.len(), if developer { 2 } else { 1 });
        assert_eq!(vars["PATH"], "/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(
            vars.get("DEVELOPER_DIR").copied(),
            developer.then_some(DEVELOPER)
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/")));
    }
}
#[test]
fn entry_guards_reject_nonroot_nonmac_and_arguments() {
    assert!(guards(false, 0, 1).is_err());
    assert!(guards(true, 501, 1).is_err());
    assert!(guards(true, 0, 2).is_err());
    assert!(guards(true, 0, 1).is_ok());
}
struct Mock {
    events: RefCell<Vec<String>>,
    bad_metadata: bool,
    fail_postverify: bool,
    root: PathBuf,
}
impl Operations for Mock {
    fn inspect(&self, path: &Path, _: Option<&Identity>, mutate: bool) -> Result<()> {
        let mut events = self.events.borrow_mut();
        if mutate {
            assert_ne!(path, Path::new("/"));
            assert_ne!(path, Path::new("/Applications"));
        }
        if self.fail_postverify
            && !mutate
            && path == self.root
            && events.iter().any(|e| e == "mutate")
        {
            return Err("postverify failed".into());
        }
        events.push(if mutate { "mutate" } else { "inspect" }.into());
        Ok(())
    }
    fn metadata(&self, _: &Path) -> Result<()> {
        self.events.borrow_mut().push("metadata".into());
        require(!self.bad_metadata, "metadata mismatch")
    }
    fn link(&self, _: &Path, _: &Entry, mutate: bool, require_root: bool) -> Result<()> {
        self.events.borrow_mut().push(
            match (mutate, require_root) {
                (false, false) => "link-preflight",
                (true, true) => "link-mutate",
                (false, true) => "link-verify",
                _ => panic!("mutating link must verify ownership"),
            }
            .into(),
        );
        Ok(())
    }
    fn detach(&self, path: &Path, entry: &Entry, parent: &Entry) -> Result<Identity> {
        self.events.borrow_mut().push("detach".into());
        detach::detach(path, &entry.id, entry.size, &parent.id)
    }
    fn select(&self) -> Result<()> {
        self.events.borrow_mut().push("select".into());
        Ok(())
    }
}
#[test]
fn wrong_metadata_stops_before_any_mutation_or_selection() {
    let (_temp, root) = tree();
    let ops = Mock {
        events: RefCell::new(vec![]),
        bad_metadata: true,
        fail_postverify: false,
        root: root.clone(),
    };
    assert!(normalize(&root, &ops).is_err());
    let events = ops.events.borrow();
    assert!(!events.iter().any(|e| e == "mutate" || e == "select"));
}
#[test]
fn selection_requires_independent_postverification() {
    let (_temp, root) = tree();
    for fail in [true, false] {
        let ops = Mock {
            events: RefCell::new(vec![]),
            bad_metadata: false,
            fail_postverify: fail,
            root: root.clone(),
        };
        assert_eq!(normalize(&root, &ops).is_err(), fail);
        let events = ops.events.borrow();
        assert_eq!(events.last().map(String::as_str) == Some("select"), !fail);
        if !fail {
            assert_eq!(events.iter().filter(|e| *e == "metadata").count(), 2);
        }
    }
}
#[test]
fn replanning_detects_inode_replacement() {
    let (_temp, root) = tree();
    let before = plan_tree(&root).unwrap();
    fs::rename(root.join("file"), root.join("old")).unwrap();
    fs::write(root.join("file"), "sdk").unwrap();
    assert_ne!(before, plan_tree(&root).unwrap());
}
#[cfg(target_os = "macos")]
#[test]
fn plist_conversion_reads_retained_input() {
    let text = command("/usr/bin/plutil", &["-convert","json","-o","-","-"], false, b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Version</key><string>26.5</string></dict></plist>").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["Version"],
        "26.5"
    );
}

#[test]
fn links_are_preflighted_and_independently_verified() {
    let (_temp, root) = tree();
    symlink("file", root.join("link")).unwrap();
    let ops = Mock {
        events: RefCell::new(vec![]),
        bad_metadata: false,
        fail_postverify: false,
        root: root.clone(),
    };
    normalize(&root, &ops).unwrap();
    let events = ops.events.borrow();
    let index = |name: &str| events.iter().position(|event| event == name).unwrap();
    assert!(index("link-preflight") < index("mutate"));
    assert!(index("link-mutate") < index("link-verify"));
    assert!(index("link-verify") < index("select"));
}

#[test]
fn normalization_detaches_internal_and_external_hardlinks_then_requires_strict_plan() {
    let (_temp, root) = tree();
    let outside = root.parent().unwrap().join("outside");
    fs::hard_link(root.join("file"), root.join("second")).unwrap();
    fs::hard_link(root.join("file"), &outside).unwrap();
    let before = fs::metadata(&outside).unwrap();
    let ops = Mock {
        events: RefCell::new(vec![]),
        bad_metadata: false,
        fail_postverify: false,
        root: root.clone(),
    };
    normalize(&root, &ops).unwrap();
    assert!(
        plan_tree(&root)
            .unwrap()
            .values()
            .all(|entry| !entry.shared)
    );
    assert_ne!(
        identity(&fs::metadata(root.join("file")).unwrap()),
        identity(&fs::metadata(root.join("second")).unwrap())
    );
    let after = fs::metadata(&outside).unwrap();
    assert_eq!(
        (after.uid(), after.gid(), after.mode()),
        (before.uid(), before.gid(), before.mode())
    );
    assert_eq!(fs::read(outside).unwrap(), b"sdk");
    let events = ops.events.borrow();
    assert_eq!(events.iter().filter(|event| *event == "detach").count(), 2);
    assert!(
        events.iter().position(|event| event == "mutate").unwrap()
            < events.iter().position(|event| event == "detach").unwrap()
    );
    assert_eq!(events.last().unwrap(), "select");
}
#[test]
fn single_link_normalization_does_not_copy() {
    let (_temp, root) = tree();
    let before = identity(&fs::metadata(root.join("file")).unwrap());
    let ops = Mock {
        events: RefCell::new(vec![]),
        bad_metadata: false,
        fail_postverify: false,
        root: root.clone(),
    };
    normalize(&root, &ops).unwrap();
    assert_eq!(before, identity(&fs::metadata(root.join("file")).unwrap()));
    assert!(!ops.events.borrow().iter().any(|event| event == "detach"));
}
#[test]
fn oversized_shared_file_is_rejected_before_any_mutation() {
    let (_temp, root) = tree();
    File::options()
        .write(true)
        .open(root.join("file"))
        .unwrap()
        .set_len(detach::MAX_FILE_BYTES + 1)
        .unwrap();
    fs::hard_link(root.join("file"), root.join("second")).unwrap();
    let ops = Mock {
        events: RefCell::new(vec![]),
        bad_metadata: false,
        fail_postverify: false,
        root: root.clone(),
    };
    assert!(normalize(&root, &ops).unwrap_err().contains("byte limit"));
    assert!(
        !ops.events
            .borrow()
            .iter()
            .any(|event| event == "mutate" || event == "detach" || event == "select")
    );
}
