mod support;

use support::canonical_tempdir;

use std::fs;
use std::process::Command;

use base64::Engine as _;

use kio_core::cas::ContentObjectKind;
use kio_core::scope::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn command(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .arg("--json")
        .args(args);
    command
}

fn ok(dir: &TempDir, args: &[&str]) -> Value {
    let output = command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=")
        .unwrap()
}

fn dimension_bomb_png() -> Vec<u8> {
    let mut bytes = png();
    bytes[16..20].copy_from_slice(&8193_u32.to_be_bytes());
    bytes
}

#[test]
fn standalone_png_is_an_image_object_and_normalized_reference_never_binary_text() {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    let bytes = png();
    fs::write(dir.path().join("pixel[proof].png"), &bytes).unwrap();

    let indexed = ok(&dir, &["index", "--offline"]);
    assert_eq!(indexed["normalized_files"], 1);
    assert_eq!(ok(&dir, &["index", "--offline"])["normalized_files"], 1);

    let repo = Repository::open(dir.path()).unwrap();
    let hash = kio_pipeline::prepare::hash_bytes(&bytes);
    assert_eq!(
        repo.object_store()
            .read_content_object_bytes(ContentObjectKind::Image, &hash, bytes.len() as u64)
            .unwrap(),
        bytes
    );

    let results = ok(&dir, &["search", "pixel", "--mode", "text"]);
    let result = results["results"].as_array().unwrap().first().unwrap();
    let images = result["related_images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["order"], 0);
    assert_eq!(
        images[0]["image_uri"],
        format!(
            "kio://{}/object/image/{hash}",
            repo.scope_identity().unwrap().scope_id
        )
    );
}

#[test]
fn malformed_or_oversized_image_never_publishes_image_cas() {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    let bytes = dimension_bomb_png();
    let hash = kio_pipeline::prepare::hash_bytes(&bytes);
    fs::write(dir.path().join("bomb.png"), &bytes).unwrap();
    let output = command(&dir, &["index", "--offline"]).output().unwrap();
    assert!(!output.status.success());
    let repo = Repository::open(dir.path()).unwrap();
    assert!(
        repo.object_store()
            .inspect_content_object(ContentObjectKind::Image, &hash)
            .is_err()
    );
    assert!(
        repo.object_store()
            .inspect_content_object(ContentObjectKind::Prepared, &hash)
            .is_err()
    );
}

#[test]
fn truncated_and_crc_invalid_pngs_publish_no_image_objects() {
    for (name, bytes) in [
        ("truncated.png", png()[..20].to_vec()),
        ("crc.png", {
            let mut bytes = png();
            bytes[29] ^= 0x01;
            bytes
        }),
    ] {
        let dir = canonical_tempdir();
        ok(&dir, &["init"]);
        let hash = kio_pipeline::prepare::hash_bytes(&bytes);
        fs::write(dir.path().join(name), &bytes).unwrap();
        assert!(
            !command(&dir, &["index", "--offline"])
                .output()
                .unwrap()
                .status
                .success()
        );
        let repo = Repository::open(dir.path()).unwrap();
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &hash)
                .is_err()
        );
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Prepared, &hash)
                .is_err()
        );
    }
}
