//! Explicit hash-authorized retry of interrupted CAS removals.
#![cfg(any(unix, windows))]

use std::{
    fs,
    path::{Path, PathBuf},
};
#[cfg(debug_assertions)]
use std::{
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use kio_core::{
    Result,
    cas::{ChunkObject, ContentObjectKind, EmbeddingObject, ObjectKind, ObjectStore, hash_bytes},
    test_control::{DebugTestControl, install_scoped},
};

const KINDS: [&str; 5] = ["raw", "chunk", "prepared", "image", "embedding"];

fn store(root: &Path, bound: bool) -> ObjectStore {
    let kio = root.join(".kio");
    if bound {
        let handle =
            cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority())
                .unwrap();
        ObjectStore::from_bound_kio(&handle).unwrap()
    } else {
        ObjectStore::new(kio)
    }
}

fn seed(root: &Path, kind: &str) -> (String, PathBuf) {
    let kio = root.join(".kio");
    for namespace in ["raw", "trees", "commits"] {
        fs::create_dir_all(kio.join("objects").join(namespace)).unwrap();
    }
    let objects = store(root, false);
    let bytes = b"authorized target plaintext";
    let hash = match kind {
        "raw" => objects.write_raw(bytes).unwrap(),
        "chunk" => objects
            .write_chunk(&ChunkObject {
                spec_version: 1,
                raw_hash: hash_bytes(b"raw"),
                tool_profile_hash: hash_bytes(b"tool"),
                r#gen: 1,
                unit_key: "test:removal".into(),
                unit_content_hash: hash_bytes(bytes),
                heading_path: vec!["Recovery".into()],
                section_id: None,
                byte_start: 0,
                byte_end: bytes.len() as u64,
                text_hash: hash_bytes(bytes),
                text: String::from_utf8(bytes.to_vec()).unwrap(),
            })
            .unwrap(),
        "embedding" => objects
            .write_embedding(&EmbeddingObject {
                spec_version: 1,
                target_type: "chunk".into(),
                target_hash: hash_bytes(b"chunk"),
                profile_hash: hash_bytes(b"profile"),
                modality: "text".into(),
                dimensions: 2,
                distance: "cosine".into(),
                context: None,
                vector: vec![1.0, 0.0],
            })
            .unwrap(),
        "prepared" => objects
            .write_content_object(ContentObjectKind::Prepared, bytes)
            .unwrap(),
        "image" => objects
            .write_content_object(ContentObjectKind::Image, bytes)
            .unwrap(),
        _ => panic!("unknown kind"),
    };
    let path = match kind {
        "raw" => objects.object_path(ObjectKind::Raw, &hash),
        "chunk" => objects.chunk_path(&hash),
        "embedding" => objects.embedding_path(&hash),
        "prepared" => objects.content_path(ContentObjectKind::Prepared, &hash),
        "image" => objects.content_path(ContentObjectKind::Image, &hash),
        _ => unreachable!(),
    }
    .unwrap();
    (hash, path)
}

fn remove(objects: &ObjectStore, kind: &str, hash: &str) -> Result<bool> {
    match kind {
        "raw" => objects.remove_raw(hash),
        "chunk" => objects.remove_chunk(hash),
        "embedding" => objects.remove_embedding(hash),
        "prepared" => objects.remove_content(ContentObjectKind::Prepared, hash),
        "image" => objects.remove_content(ContentObjectKind::Image, hash),
        _ => panic!("unknown kind"),
    }
}

fn quarantine(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".kio-cas-remove-{}",
        path.file_name().unwrap().to_str().unwrap()
    ))
}

#[test]
fn missing_canonical_retries_semantically_verified_quarantine_for_every_kind() {
    for bound in [false, true] {
        for kind in KINDS {
            let fixture = tempfile::tempdir().unwrap();
            let (hash, path) = seed(fixture.path(), kind);
            if matches!(kind, "chunk" | "embedding") {
                assert_ne!(hash_bytes(&fs::read(&path).unwrap()), hash);
            }
            let removed = quarantine(&path);
            fs::rename(&path, &removed).unwrap();
            let objects = store(fixture.path(), bound);
            assert!(remove(&objects, kind, &hash).unwrap(), "{kind}, {bound}");
            assert!(!path.exists());
            assert!(!removed.exists());
            assert!(!remove(&objects, kind, &hash).unwrap());
            assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
        }
    }
}

#[test]
fn wrong_quarantine_bytes_and_ambiguous_names_fail_without_deleting_either() {
    for bound in [false, true] {
        for kind in KINDS {
            for both_names in [false, true] {
                let fixture = tempfile::tempdir().unwrap();
                let (hash, path) = seed(fixture.path(), kind);
                let removed = quarantine(&path);
                let original = fs::read(&path).unwrap();
                fs::rename(&path, &removed).unwrap();
                if both_names {
                    // Even two independently valid representations are ambiguous.
                    fs::write(&path, &original).unwrap();
                } else {
                    fs::write(&removed, b"unattributed bytes").unwrap();
                }
                assert!(remove(&store(fixture.path(), bound), kind, &hash).is_err());
                assert_eq!(path.exists(), both_names);
                assert_eq!(
                    fs::read(&removed).unwrap(),
                    if both_names {
                        original.clone()
                    } else {
                        b"unattributed bytes".to_vec()
                    }
                );
                if both_names {
                    assert_eq!(fs::read(&path).unwrap(), original);
                }
            }
        }
    }
}

#[test]
fn valid_semantic_object_for_another_key_is_not_recovery_authority() {
    for bound in [false, true] {
        for kind in ["chunk", "embedding"] {
            let fixture = tempfile::tempdir().unwrap();
            let (hash, path) = seed(fixture.path(), kind);
            let objects = store(fixture.path(), false);
            let original = fs::read(&path).unwrap();
            let alternate_path = if kind == "chunk" {
                let mut other: ChunkObject = serde_json::from_slice(&original).unwrap();
                other.raw_hash = hash_bytes(b"different raw identity");
                let other_hash = objects.write_chunk(&other).unwrap();
                assert_ne!(other_hash, hash);
                objects.chunk_path(&other_hash).unwrap()
            } else {
                let mut other = EmbeddingObject::from_bytes(&original).unwrap();
                other.target_hash = hash_bytes(b"different chunk identity");
                let other_hash = objects.write_embedding(&other).unwrap();
                assert_ne!(other_hash, hash);
                objects.embedding_path(&other_hash).unwrap()
            };
            let removed = quarantine(&path);
            let unrelated = fs::read(&alternate_path).unwrap();
            fs::rename(&path, &removed).unwrap();
            fs::write(&removed, &unrelated).unwrap();
            assert!(remove(&store(fixture.path(), bound), kind, &hash).is_err());
            assert_eq!(fs::read(&removed).unwrap(), unrelated);
            assert_eq!(fs::read(&alternate_path).unwrap(), unrelated);
            assert!(!path.exists());
        }
    }
}

#[test]
fn linked_quarantine_is_rejected_and_unknown_siblings_are_preserved() {
    for bound in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (hash, path) = seed(fixture.path(), "raw");
        let removed = quarantine(&path);
        let unrelated = path.with_file_name(".purge-remove-legacy-unknown");
        fs::rename(&path, &removed).unwrap();
        fs::hard_link(&removed, &unrelated).unwrap();
        let objects = store(fixture.path(), bound);
        assert!(objects.remove_raw(&hash).is_err());
        assert!(removed.exists());
        assert!(unrelated.exists());
        fs::remove_file(&unrelated).unwrap();
        fs::write(&unrelated, b"unattributed legacy residue").unwrap();
        assert!(objects.remove_raw(&hash).unwrap());
        assert_eq!(fs::read(unrelated).unwrap(), b"unattributed legacy residue");
    }
}

#[cfg(unix)]
#[test]
fn symlink_quarantine_is_rejected_without_touching_target() {
    for bound in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (hash, path) = seed(fixture.path(), "raw");
        let removed = quarantine(&path);
        let external = fixture.path().join("external");
        fs::rename(&path, &external).unwrap();
        std::os::unix::fs::symlink(&external, &removed).unwrap();
        assert!(store(fixture.path(), bound).remove_raw(&hash).is_err());
        assert_eq!(fs::read(external).unwrap(), b"authorized target plaintext");
        assert!(fs::symlink_metadata(removed).unwrap().is_symlink());
    }
}

#[cfg(debug_assertions)]
struct KillOnDrop(Child);
#[cfg(debug_assertions)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cas_removal_crash_child() {
    let Some(root) = std::env::var_os("KIO_CAS_CRASH_ROOT") else {
        return;
    };
    let kind = std::env::var("KIO_CAS_CRASH_KIND").unwrap();
    let hash = std::env::var("KIO_CAS_CRASH_HASH").unwrap();
    let bound = std::env::var("KIO_CAS_CRASH_BOUND").unwrap() == "true";
    let _control = install_scoped(DebugTestControl::from_env());
    remove(&store(Path::new(&root), bound), &kind, &hash).unwrap();
    panic!("child should stop at the selected durability barrier");
}

#[cfg(debug_assertions)]
#[test]
fn abrupt_stop_at_each_removal_transition_is_retryable() {
    for bound in [false, true] {
        for kind in KINDS {
            for point in [
                "cas_remove_ready",
                "cas_remove_quarantined",
                "cas_remove_deleted",
            ] {
                let fixture = tempfile::tempdir().unwrap();
                let (hash, path) = seed(fixture.path(), kind);
                let ready = fixture.path().join("ready");
                let stderr = fs::File::create(fixture.path().join("child.stderr")).unwrap();
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "cas_removal_crash_child", "--test-threads=1"])
                    .env_clear()
                    .env("KIO_CAS_CRASH_ROOT", fixture.path())
                    .env("KIO_CAS_CRASH_KIND", kind)
                    .env("KIO_CAS_CRASH_HASH", &hash)
                    .env("KIO_CAS_CRASH_BOUND", bound.to_string())
                    .env("KIO_TEST_DURABILITY_POINT", point)
                    .env("KIO_TEST_DURABILITY_READY", &ready)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(stderr));
                #[cfg(windows)]
                if let Some(system_root) = std::env::var_os("SystemRoot") {
                    command.env("SystemRoot", system_root);
                }
                let mut child = KillOnDrop(command.spawn().unwrap());
                let expected = format!("point={point}\npid={}\n", child.0.id());
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if fs::read_to_string(&ready).ok().as_deref() == Some(&expected) {
                        break;
                    }
                    assert!(
                        child.0.try_wait().unwrap().is_none(),
                        "child exited before {point}"
                    );
                    assert!(Instant::now() < deadline, "child did not reach {point}");
                    thread::sleep(Duration::from_millis(10));
                }
                child.0.kill().unwrap();
                assert!(!child.0.wait().unwrap().success());
                let removed = quarantine(&path);
                assert_eq!(path.exists(), point == "cas_remove_ready");
                assert_eq!(removed.exists(), point == "cas_remove_quarantined");
                let objects = store(fixture.path(), bound);
                assert_eq!(
                    remove(&objects, kind, &hash).unwrap(),
                    point != "cas_remove_deleted"
                );
                assert!(!remove(&objects, kind, &hash).unwrap());
                assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
            }
        }
    }
}
