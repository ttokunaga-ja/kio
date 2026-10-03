#![cfg(any(unix, windows))]

use std::io::Cursor;

use kio_core::cas::{
    ChunkObject, ContentObjectKind, EmbeddingObject, ObjectKind, ObjectStore, hash_bytes,
};

fn chunk(text: &str) -> ChunkObject {
    ChunkObject {
        spec_version: 1,
        raw_hash: format!("sha256:{}", "a".repeat(64)),
        tool_profile_hash: format!("sha256:{}", "b".repeat(64)),
        r#gen: 1,
        unit_key: "bound:test".to_owned(),
        unit_content_hash: format!("sha256:{}", "c".repeat(64)),
        heading_path: vec!["Bound".to_owned()],
        section_id: None,
        byte_start: 0,
        byte_end: text.len() as u64,
        text_hash: hash_bytes(text.as_bytes()),
        text: text.to_owned(),
    }
}

fn embedding() -> EmbeddingObject {
    EmbeddingObject {
        spec_version: 1,
        target_type: "chunk".to_owned(),
        target_hash: format!("sha256:{}", "d".repeat(64)),
        profile_hash: format!("sha256:{}", "e".repeat(64)),
        modality: "text".to_owned(),
        dimensions: 2,
        distance: "cosine".to_owned(),
        context: None,
        vector: vec![1.0, 0.0],
    }
}

#[test]
fn bound_content_inspection_charges_corrupt_bytes_to_the_read_budget() {
    let fixture = tempfile::tempdir().unwrap();
    let kio = fixture.path().join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let retained =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority()).unwrap();
    let store = ObjectStore::from_bound_kio(&retained).unwrap();
    let bytes = vec![b'i'; 256 * 1024 + 7];
    let hash = store
        .write_content_object(ContentObjectKind::Image, &bytes)
        .unwrap();
    assert_eq!(
        store
            .inspect_content_accounted(ContentObjectKind::Image, &hash)
            .unwrap()
            .size_bytes,
        bytes.len() as u64
    );
    let path = ObjectStore::new(&kio)
        .content_path(ContentObjectKind::Image, &hash)
        .unwrap();
    std::fs::write(path, vec![b'x'; bytes.len()]).unwrap();
    let error = store
        .inspect_content_accounted(ContentObjectKind::Image, &hash)
        .unwrap_err();
    assert_eq!(error.error.error_code(), "KIO-E-STORE-CORRUPT-001");
    assert_eq!(error.consumed_bytes, bytes.len() as u64);
}

#[test]
fn bound_inspection_accounted_reads_and_streaming_copy_follow_retained_namespaces() {
    let fixture = tempfile::tempdir().unwrap();
    let kio = fixture.path().join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let retained =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority()).unwrap();
    #[cfg(windows)]
    let retained = kio_core::store_dir::StoreDirectory::from_retained(retained, kio.clone())
        .unwrap()
        .root_handle();
    let store = ObjectStore::from_bound_kio(&retained).unwrap();
    let mut objects = Vec::new();
    let raw = vec![b'z'; 128 * 1024 + 17];
    objects.push((ObjectKind::Raw, store.write_raw(&raw).unwrap(), raw));
    for kind in [ObjectKind::Tree, ObjectKind::Commit] {
        let value = serde_json::json!({"namespace":format!("{kind:?}")});
        let (hash, bytes) = store.write_json(kind, &value).unwrap();
        objects.push((kind, hash, bytes));
    }
    let original = fixture.path().join("retained");
    std::fs::rename(&kio, &original).unwrap();
    std::fs::create_dir(&kio).unwrap();
    for (kind, hash, bytes) in &objects {
        let metadata = store.inspect_by_hash(hash).unwrap();
        assert_eq!(metadata.kind, *kind);
        assert_eq!(metadata.size_bytes, bytes.len() as u64);
        assert_eq!(
            store.inspect_object(*kind, hash).unwrap().size_bytes,
            bytes.len() as u64
        );
        assert_eq!(
            store
                .inspect_object_accounted(*kind, hash)
                .unwrap()
                .size_bytes,
            bytes.len() as u64
        );
        let (object, consumed) = store.read_object_accounted(*kind, hash).unwrap();
        assert_eq!(object.bytes, *bytes);
        assert_eq!(consumed, bytes.len() as u64);
        let mut copied = Vec::new();
        assert_eq!(
            store
                .copy_object_to(*kind, hash, &mut copied)
                .unwrap()
                .size_bytes,
            bytes.len() as u64
        );
        assert_eq!(copied, *bytes);
    }
    assert_eq!(
        std::fs::read_dir(&kio).unwrap().count(),
        0,
        "bound reads must not use the substituted public store"
    );

    let (kind, hash, bytes) = &objects[0];
    let original_store = ObjectStore::new(original);
    std::fs::write(
        original_store.object_path(*kind, hash).unwrap(),
        vec![b'x'; bytes.len()],
    )
    .unwrap();
    let error = store.inspect_object_accounted(*kind, hash).unwrap_err();
    assert_eq!(error.error.error_code(), "KIO-E-STORE-CORRUPT-001");
    assert_eq!(
        error.consumed_bytes,
        bytes.len() as u64,
        "corrupt reads still consume the fsck budget"
    );
}

#[test]
fn retained_store_stages_publishes_and_reads_without_a_public_kio_path() {
    let fixture = tempfile::tempdir().expect("temporary scope");
    let kio = fixture.path().join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).expect("object namespace");
    }

    let retained_kio =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority())
            .expect("retained .kio directory");
    let store = ObjectStore::from_bound_kio(&retained_kio).expect("bound store");

    let bytes = b"bound raw stage";
    let mut source = Cursor::new(bytes.as_slice());
    let stage = store
        .stage_raw_from_reader(&mut source, bytes.len() as u64)
        .expect("stage raw");
    let expected = stage.raw_hash().to_owned();
    assert_eq!(stage.size_bytes(), bytes.len() as u64);
    assert_eq!(
        store.publish_bound_raw_stage(stage).expect("publish"),
        (expected.clone(), bytes.len() as u64)
    );
    assert_eq!(
        store
            .read_object(ObjectKind::Raw, &expected)
            .expect("read")
            .bytes,
        bytes
    );

    store
        .validate_bound_layout()
        .expect("unchanged retained layout");
}

#[test]
fn retained_repairs_removals_and_embedding_inventory_never_follow_public_substitution() {
    let fixture = tempfile::tempdir().unwrap();
    let kio = fixture.path().join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let retained =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority()).unwrap();
    #[cfg(windows)]
    let retained = kio_core::store_dir::StoreDirectory::from_retained(retained, kio.clone())
        .unwrap()
        .root_handle();
    let store = ObjectStore::from_bound_kio(&retained).unwrap();
    let raw_bytes = b"raw retained repair";
    let raw_hash = store.write_raw(raw_bytes).unwrap();
    let chunk = chunk("retained delete");
    let chunk_hash = store.write_chunk(&chunk).unwrap();
    let embedding = embedding();
    let embedding_hash = store.write_embedding(&embedding).unwrap();

    let parked = fixture.path().join("retained");
    std::fs::rename(&kio, &parked).unwrap();
    std::fs::create_dir(&kio).unwrap();
    let plain = ObjectStore::new(&parked);
    std::fs::write(
        plain.object_path(ObjectKind::Raw, &raw_hash).unwrap(),
        b"corrupt retained raw",
    )
    .unwrap();

    assert!(store.repair_raw(&raw_hash, raw_bytes).unwrap());
    assert_eq!(
        store.read_object(ObjectKind::Raw, &raw_hash).unwrap().bytes,
        raw_bytes
    );
    assert_eq!(
        store.embedding_hashes().unwrap(),
        vec![embedding_hash.clone()]
    );
    assert!(store.remove_chunk(&chunk_hash).unwrap());
    assert!(store.remove_embedding(&embedding_hash).unwrap());
    assert!(store.remove_raw(&raw_hash).unwrap());
    assert!(!store.remove_raw(&raw_hash).unwrap());
    assert!(store.embedding_hashes().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(&kio).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn store_owned_kio_clone_survives_moves_after_raw_caller_handle_is_closed() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("scope");
    let kio = root.join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    // Deliberately pass a raw named handle, without fixture normalization.
    let caller =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority()).unwrap();
    let store = ObjectStore::from_bound_kio(&caller).unwrap();
    drop(caller);
    let bytes = b"raw bytes from the original bound store";
    let hash = store.write_raw(bytes).unwrap();
    let image = b"image bytes from the original lazy namespace";
    let image_hash = store
        .write_content_object(ContentObjectKind::Image, image)
        .unwrap();

    std::fs::rename(&kio, root.join("retained-kio")).unwrap();
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let replacement = ObjectStore::new(&kio);
    let replacement_hash = replacement.write_raw(b"replacement bytes").unwrap();
    assert!(replacement.read_object(ObjectKind::Raw, &hash).is_err());
    assert!(
        store
            .read_object(ObjectKind::Raw, &replacement_hash)
            .is_err()
    );
    assert_eq!(
        store.read_object(ObjectKind::Raw, &hash).unwrap().bytes,
        bytes
    );
    assert_eq!(
        store
            .read_content_object_bytes(ContentObjectKind::Image, &image_hash, 4096)
            .unwrap(),
        image
    );

    std::fs::rename(&root, fixture.path().join("retained-scope")).unwrap();
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let replacement = ObjectStore::new(&kio);
    let replacement_hash = replacement.write_raw(b"replacement root bytes").unwrap();
    assert!(replacement.read_object(ObjectKind::Raw, &hash).is_err());
    assert!(
        store
            .read_object(ObjectKind::Raw, &replacement_hash)
            .is_err()
    );
    assert_eq!(
        store.read_object(ObjectKind::Raw, &hash).unwrap().bytes,
        bytes
    );
    assert_eq!(
        store
            .read_content_object_bytes(ContentObjectKind::Image, &image_hash, 4096)
            .unwrap(),
        image
    );
    assert_eq!(
        replacement
            .read_object(ObjectKind::Raw, &replacement_hash)
            .unwrap()
            .bytes,
        b"replacement root bytes"
    );
}

#[cfg(unix)]
#[test]
fn retained_removal_rejects_a_hardlinked_raw_leaf_without_unlinking_either_name() {
    let fixture = tempfile::tempdir().unwrap();
    let kio = fixture.path().join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let retained =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority()).unwrap();
    let store = ObjectStore::from_bound_kio(&retained).unwrap();
    let bytes = b"bound hardlink removal";
    let hash = store.write_raw(bytes).unwrap();
    let parked = fixture.path().join("retained");
    std::fs::rename(&kio, &parked).unwrap();
    std::fs::create_dir(&kio).unwrap();
    let path = ObjectStore::new(&parked)
        .object_path(ObjectKind::Raw, &hash)
        .unwrap();
    let outside = fixture.path().join("outside-hardlink");
    std::fs::hard_link(&path, &outside).unwrap();

    assert_eq!(
        store.remove_raw(&hash).unwrap_err().error_code(),
        "KIO-E-STORE-CORRUPT-001"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read(&outside).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&kio).unwrap().count(), 0);
}

#[test]
fn retained_store_late_binds_peer_created_semantic_namespaces_without_ambient_fallback() {
    let fixture = tempfile::tempdir().unwrap();
    let kio = fixture.path().join(".kio");
    for kind in ["raw", "trees", "commits"] {
        std::fs::create_dir_all(kio.join("objects").join(kind)).unwrap();
    }
    let retained =
        cap_primitives::fs::open_ambient_dir(&kio, cap_primitives::ambient_authority()).unwrap();
    let bound = ObjectStore::from_bound_kio(&retained).unwrap();
    let peer = ObjectStore::new(&kio);
    let chunk = chunk("late bound");
    let chunk_hash = peer.write_chunk(&chunk).unwrap();
    let embedding = embedding();
    let embedding_hash = peer.write_embedding(&embedding).unwrap();

    assert_eq!(bound.read_chunk(&chunk_hash).unwrap(), chunk);
    assert_eq!(
        bound.embedding_hashes().unwrap(),
        vec![embedding_hash.clone()]
    );
    assert!(bound.remove_chunk(&chunk_hash).unwrap());
    assert!(bound.remove_embedding(&embedding_hash).unwrap());
}
