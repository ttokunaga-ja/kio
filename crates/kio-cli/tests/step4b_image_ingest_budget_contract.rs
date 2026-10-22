//! End-to-end image-ingest allocation-budget contract.

mod support;

use assert_cmd::Command;
use kio_core::cas::{ContentObjectKind, ObjectStore, hash_bytes};
use kio_core::scope::Repository;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

use support::canonical_tempdir;

const SAFE_WEBP: &[u8] = include_bytes!("fixtures/vp8l-id65535-single.webp");
const OVERBUDGET_WEBP: &[u8] = include_bytes!("fixtures/vp8l-id65535-green256.webp");

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_OFFICE_CONVERT",
    "KIO_OFFICE_CONVERTER",
];

fn kio(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args);
    command
}

fn initialize(dir: &TempDir, filename: &str, bytes: &[u8]) {
    fs::write(dir.path().join(filename), bytes).unwrap();
    kio(dir, &["init"]).assert().success();
    // Local/deterministic execution still requires explicit physical ledger
    // initialization under the current all-profiles contract.
    kio(dir, &["ledger", "init"]).assert().success();
}

fn index_output(dir: &TempDir) -> std::process::Output {
    kio(dir, &["index", "--offline", "--yes"])
        .arg("--json")
        .output()
        .unwrap()
}

fn kio_dir(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".kio")
}

#[test]
fn vp8l_tree_budget_is_enforced_at_native_webp_ingest_before_cas_authority() {
    assert_eq!(
        hash_bytes(SAFE_WEBP),
        "sha256:0d72009af93571ab331247fe2fce8105aa6751a40019cda13d8f82efee37bd5a"
    );
    assert_eq!(
        hash_bytes(OVERBUDGET_WEBP),
        "sha256:1d8d706cf64d5deea76f0ee2fd750f7d63e1d885c4b76a7231ef9c0372b0fd11"
    );

    let safe = canonical_tempdir();
    initialize(&safe, "safe.webp", SAFE_WEBP);
    let safe_output = index_output(&safe);
    assert!(
        safe_output.status.success(),
        "{}",
        String::from_utf8_lossy(&safe_output.stderr)
    );
    let safe_index: Value = serde_json::from_slice(&safe_output.stdout).unwrap();
    assert_eq!(safe_index["failed_files"], 0, "{safe_index}");
    assert_eq!(safe_index["normalized_files"], 1, "{safe_index}");

    let safe_repo = Repository::open(safe.path()).unwrap();
    let safe_head = safe_repo.head_commit_hash().unwrap().unwrap();
    let safe_commit = safe_repo.read_commit(&safe_head).unwrap();
    let safe_entry = safe_repo
        .read_tree(&safe_commit.tree)
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.path == "safe.webp")
        .expect("the accepted standalone WebP must be in the published tree");
    assert!(safe_entry.normalize.is_some());
    let safe_hash = hash_bytes(SAFE_WEBP);
    assert_eq!(safe_entry.raw_hash, safe_hash);
    let safe_store = ObjectStore::new(kio_dir(&safe));
    assert_eq!(
        safe_store
            .read_content_object_bytes(
                ContentObjectKind::Image,
                &safe_hash,
                SAFE_WEBP.len() as u64,
            )
            .unwrap(),
        SAFE_WEBP,
        "accepted source CAS must preserve the exact original WebP bytes"
    );

    let unsafe_dir = canonical_tempdir();
    initialize(&unsafe_dir, "overbudget.webp", OVERBUDGET_WEBP);
    let unsafe_output = index_output(&unsafe_dir);
    // Native image admission propagates KioError::schema before publication
    // (06 §§7–8). Require that normal classified failure: a crash, signal,
    // Windows NTSTATUS or unrelated ledger/policy refusal is not budget Green.
    assert_eq!(
        unsafe_output.status.code(),
        Some(2),
        "over-budget source must return the existing schema-failure exit: stdout={} stderr={}",
        String::from_utf8_lossy(&unsafe_output.stdout),
        String::from_utf8_lossy(&unsafe_output.stderr)
    );
    assert!(unsafe_output.stdout.is_empty());
    let refused: Value = serde_json::from_slice(&unsafe_output.stderr)
        .expect("image admission refusal must be a normal CLI JSON error");
    assert_eq!(
        refused["error_code"], "KIO-E-CONFIG-SCHEMA-001",
        "{refused}"
    );

    let unsafe_hash = hash_bytes(OVERBUDGET_WEBP);
    let unsafe_store = ObjectStore::new(kio_dir(&unsafe_dir));
    assert!(
        unsafe_store
            .inspect_content_accounted(ContentObjectKind::Image, &unsafe_hash)
            .is_err(),
        "refused input must not gain image CAS authority"
    );
    assert!(
        unsafe_store
            .inspect_content_accounted(ContentObjectKind::Prepared, &unsafe_hash)
            .is_err(),
        "refused input must not gain prepared CAS authority"
    );

    let unsafe_repo = Repository::open(unsafe_dir.path()).unwrap();
    if let Some(unsafe_head) = unsafe_repo.head_commit_hash().unwrap() {
        let unsafe_commit = unsafe_repo.read_commit(&unsafe_head).unwrap();
        let unsafe_tree = unsafe_repo.read_tree(&unsafe_commit.tree).unwrap();
        assert!(
            unsafe_tree
                .entries
                .iter()
                .find(|entry| entry.path == "overbudget.webp")
                .is_none_or(|entry| entry.normalize.is_none()),
            "refused input must not acquire normalized tree authority"
        );
    }

    // Offline mode prevents online OCR/embed transport in this native-ingest
    // test. A failure task may be recorded; task absence is not asserted. A
    // separate mocked-online test must assert zero send/ledger use.
}

#[test]
fn lossy_vp8_decoded_allocations_are_refused_before_cas_authority() {
    let bytes = include_bytes!("fixtures/vp8-solid-4000.webp");
    assert_eq!(
        hash_bytes(bytes),
        "sha256:c9e3a475df5b6f9795db06162a979756259742166284a824365003f9880e1d9b"
    );
    let dir = canonical_tempdir();
    initialize(&dir, "overbudget.webp", bytes);
    let output = index_output(&dir);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    let store = ObjectStore::new(kio_dir(&dir));
    for kind in [ContentObjectKind::Image, ContentObjectKind::Prepared] {
        assert!(
            store
                .inspect_content_accounted(kind, &hash_bytes(bytes))
                .is_err()
        );
    }
    let repo = Repository::open(dir.path()).unwrap();
    if let Some(head) = repo.head_commit_hash().unwrap() {
        let commit = repo.read_commit(&head).unwrap();
        assert!(
            repo.read_tree(&commit.tree)
                .unwrap()
                .entries
                .iter()
                .find(|entry| entry.path == "overbudget.webp")
                .is_none_or(|entry| entry.normalize.is_none())
        );
    }
}

#[test]
fn jpeg_decoder_owned_metadata_and_encoded_copy_share_the_decoded_budget() {
    let safe = include_bytes!("fixtures/jpeg-icc-one-control.jpg");
    let unsafe_bytes = include_bytes!("fixtures/jpeg-icc-400000-overbudget.jpg");
    assert_eq!(
        hash_bytes(safe),
        "sha256:bb72dc87a9dfef5f4211904606b85bee5689ed83ca59e506c91a6c494269a08f"
    );
    assert_eq!(
        hash_bytes(unsafe_bytes),
        "sha256:e160cc81bf5e1e3a1c59b0426c5eb48cb343761b56509205764692f610182c93"
    );
    let safe_dir = canonical_tempdir();
    initialize(&safe_dir, "control.jpg", safe);
    let accepted = index_output(&safe_dir);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let report: Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(report["failed_files"], 0, "{report}");
    assert_eq!(report["normalized_files"], 1, "{report}");
    let safe_store = ObjectStore::new(kio_dir(&safe_dir));
    assert_eq!(
        safe_store
            .read_content_object_bytes(
                ContentObjectKind::Image,
                &hash_bytes(safe),
                safe.len() as u64
            )
            .unwrap(),
        safe
    );

    let dir = canonical_tempdir();
    initialize(&dir, "overbudget.jpg", unsafe_bytes);
    let refused = index_output(&dir);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "decoder-owned ICC records plus encoded copy and RGB output exceed 64 MiB: stdout={} stderr={}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(refused.stdout.is_empty());
    let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    let store = ObjectStore::new(kio_dir(&dir));
    for kind in [ContentObjectKind::Image, ContentObjectKind::Prepared] {
        assert!(
            store
                .inspect_content_accounted(kind, &hash_bytes(unsafe_bytes))
                .is_err()
        );
    }
    let repo = Repository::open(dir.path()).unwrap();
    if let Some(head) = repo.head_commit_hash().unwrap() {
        let commit = repo.read_commit(&head).unwrap();
        assert!(
            repo.read_tree(&commit.tree)
                .unwrap()
                .entries
                .iter()
                .find(|entry| entry.path == "overbudget.jpg")
                .is_none_or(|entry| entry.normalize.is_none())
        );
    }
}
