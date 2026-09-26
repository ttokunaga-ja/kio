use std::collections::BTreeSet;

use base64::Engine as _;
use kio_adapter::types::{MarkdownizeMode as AdapterMarkdownizeMode, PreparedUnitHint, UnitKind};
use kio_adapter::{BatchOcrMaterializationError, materialize_mistral_batch_ocr_body};
use kio_core::cas::ContentObjectKind;
use kio_core::scope::Repository;
use kio_pipeline::markdownize::{
    MarkdownizeMode, load_validated_normalized_instance, persist_normalized_instance,
};
use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};
use kio_pipeline::task::RetryErrorKind;
use serde_json::json;

#[test]
fn batch_ocr_image_is_owned_reopenable_and_retained_for_its_source_unit() {
    // A real, decodable 1x1 PNG. Keep this fixture shaped like Mistral's
    // response rather than constructing an already-parsed OCR image.
    let source = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=")
        .unwrap();
    let raw_hash = hash_bytes(&source);
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&source);
    let body = json!({
        "model": "mistral-ocr-2505",
        "pages": [{
            "index": 0,
            "markdown": "![provider figure](data:image/png;base64,ignored)",
            "images": [{
                "image_base64": format!("data:image/png;base64,{encoded}"),
                "media_type": "image/png"
            }]
        }]
    });
    let hint = PreparedUnitHint {
        unit_key: "image:0".to_owned(),
        prepared_hash: raw_hash.clone(),
        unit_kind: UnitKind::Image,
        order: 0,
    };
    let (response, _) = materialize_mistral_batch_ocr_body(
        &body,
        Some(std::slice::from_ref(&hint)),
        "image/png",
        &raw_hash,
        &scope_id,
        repo.kio_dir(),
        false,
    )
    .unwrap();
    let expected_uri = format!("kio://{scope_id}/object/image/{raw_hash}");
    assert_eq!(
        response.updated_units[0].markdown,
        format!("![provider figure]({expected_uri})")
    );
    assert_eq!(
        response.updated_units[0].owned_image_hashes,
        BTreeSet::from([raw_hash.clone()])
    );
    assert_eq!(
        repo.object_store()
            .read_content_object_bytes(ContentObjectKind::Image, &raw_hash, 1024)
            .unwrap(),
        source
    );

    let prepared = PreparedUnit {
        order: 0,
        unit_key: "image:0".to_owned(),
        unit_type: UnitType::Image,
        prepared_hash: raw_hash.clone(),
        preparation_profile_hash: hash_bytes(b"prepare"),
        fingerprint: UnitFingerprint {
            perceptual_hash: hash_bytes(b"perceptual"),
            text_hash: hash_bytes(b"text"),
            visual_hash: hash_bytes(b"visual"),
        },
        mime: Some("image/png".to_owned()),
        page_number: None,
    };
    let tool_profile_hash = hash_bytes(b"markdown");
    let normalized = super::normalized_units_from_response(
        &repo,
        "image/png",
        &source,
        &response,
        std::slice::from_ref(&prepared),
        None,
        &raw_hash,
        &tool_profile_hash,
        0,
        MarkdownizeMode::Full,
        "2026-09-08T00:00:00Z",
    )
    .unwrap();
    assert_eq!(
        normalized[0].owned_image_hashes,
        BTreeSet::from([raw_hash.clone()])
    );
    assert!(normalized[0].markdown.contains(&expected_uri));
    assert_eq!(response.mode_used, AdapterMarkdownizeMode::Full);
    let manifest = super::manifest_from_units(
        std::slice::from_ref(&prepared),
        &normalized,
        &raw_hash,
        &tool_profile_hash,
        0,
        None,
        "run_batch_image",
        "2026-09-08T00:00:00Z",
        RetryErrorKind::NetworkError,
    );
    persist_normalized_instance(repo.kio_dir(), &manifest, &normalized).unwrap();

    drop(repo);
    let reopened = Repository::open(root.path()).unwrap();
    assert_eq!(
        reopened
            .object_store()
            .read_content_object_bytes(ContentObjectKind::Image, &raw_hash, 1024)
            .unwrap(),
        source
    );
    let persisted =
        load_validated_normalized_instance(reopened.kio_dir(), &raw_hash, &tool_profile_hash, 0)
            .unwrap();
    assert_eq!(
        persisted.units[0].owned_image_hashes,
        BTreeSet::from([raw_hash])
    );
}

#[test]
fn batch_ocr_existing_image_cas_corruption_is_recollectable_persistence_failure() {
    let source = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=")
        .unwrap();
    let raw_hash = hash_bytes(&source);
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let digest = raw_hash.strip_prefix("sha256:").unwrap();
    let poisoned = repo
        .kio_dir()
        .join("objects/image")
        .join(&digest[..2])
        .join(&digest[2..4])
        .join(digest);
    std::fs::create_dir_all(poisoned.parent().unwrap()).unwrap();
    std::fs::write(&poisoned, b"not the claimed image").unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&source);
    let body = json!({
        "model": "mistral-ocr-2505",
        "pages": [{
            "index": 0,
            "markdown": "![provider figure](provider.png)",
            "images": [{ "image_base64": format!("data:image/png;base64,{encoded}") }]
        }]
    });
    let hint = PreparedUnitHint {
        unit_key: "page:1".to_owned(),
        prepared_hash: hash_bytes(b"prepared"),
        unit_kind: UnitKind::Page,
        order: 0,
    };
    assert!(matches!(
        materialize_mistral_batch_ocr_body(
            &body,
            Some(&[hint]),
            "application/pdf",
            &hash_bytes(b"raw"),
            &scope_id,
            repo.kio_dir(),
            false,
        ),
        Err(BatchOcrMaterializationError::Persistence(_))
    ));
}

#[test]
fn batch_ocr_rejects_malformed_embedded_image_before_persistence() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let body = json!({
        "model": "mistral-ocr-2505",
        "pages": [{ "index": 0, "images": [{ "image_base64": "%%%" }] }]
    });
    let hint = PreparedUnitHint {
        unit_key: "page:1".to_owned(),
        prepared_hash: hash_bytes(b"prepared"),
        unit_kind: UnitKind::Page,
        order: 0,
    };
    assert!(
        materialize_mistral_batch_ocr_body(
            &body,
            Some(&[hint]),
            "application/pdf",
            &hash_bytes(b"raw"),
            &scope_id,
            repo.kio_dir(),
            false,
        )
        .is_err()
    );
}
