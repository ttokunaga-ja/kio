//! Stage 3 (S3-E): the `offline_api` markdownize route, end to end.
//!
//! A scanned PDF never reaches the deterministic baseline route — with no text
//! layer it produces no prepared units and is enqueued for enrichment instead —
//! so this is the only route where a local OCR pipeline is any use. These
//! assert that it runs there after its explicit local-peer grant, and that it
//! still creates no paid ledger row or second dispatch command.
//!
//! Every test that claims the local backend ran checks the *content*, not the
//! recorded profile. A wiring that swaps `tool_lock.json`'s markdown profile
//! while another adapter produces the text passes every profile assertion and
//! is the worse of the two failures — the archive would assert the local
//! pipeline's identity over bytes it never touched.

mod support;

use support::canonical_tempdir;

use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use assert_cmd::Command;
use kio_core::scope::Repository;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_EVAL_DETERMINISTIC_EMBED",
    "KIO_TEST_LOCAL_OCR",
    "KIO_TEST_LOCAL_OCR_BODY",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
];

/// The mock backend's own page text. Used as the witness that the local
/// pipeline produced a document's body rather than merely being recorded.
const MOCK_PAGE_TEXT: &str = "Kio local OCR mock page.";

/// Public test CA with a loopback SAN. The trust command copies it into
/// owner-private managed storage; this source is itself made owner-private so
/// registration exercises the same protected-source boundary as production.
const LOCAL_PEER_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBWTCB/6ADAgECAhR5P0J0YMFaZPlrOFyN8/jwpGzhqjAKBggqhkjOPQQDAjAh\nMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25lZCBjZXJ0MCAXDTc1MDEwMTAwMDAw\nMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25l\nZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGhIwEPQnvWSlH+iUQ5Ui\nq0khmBj4hlxmW+mTL5xjjyxwwopOnkxLs2AofasS2lYPdILzMaIeRE78g2S7Hz1n\nX6MTMBEwDwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA0GBG4uEL\nSJJea18R17+yhRTyn3rsD6PzDhdg7F/v2sQCIQCccgcgjDh+ABNajeb1deKZoqbx\nnP4qBrGe09azOI4jbg==\n-----END CERTIFICATE-----\n";

fn kio(dir: &TempDir, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

fn json_success(dir: &TempDir, args: &[&str], env: &[(&str, &str)]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let output = kio(dir, &full, env).output().unwrap();
    assert!(
        output.status.success(),
        "kio {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "kio {args:?} stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn configure_local_ocr(dir: &TempDir, env: &[(&str, &str)]) {
    // `TempDir` is rooted below the test harness's shared temporary device.
    // The trust reader requires the CA's immediate parent to be owner-only,
    // independently of the file mode.
    #[cfg(unix)]
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join(".test-config/kio/tools.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        "[markdown.paddleocr_vl_local]\nkind = \"offline_api\"\nurl = \"https://127.0.0.1:8443\"\nmodel = \"PaddleOCR-VL-0.9B\"\n",
    )
    .unwrap();
    let ca_path = dir.path().join("local-peer-ca.pem");
    fs::write(&ca_path, LOCAL_PEER_CA_PEM).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&ca_path, fs::Permissions::from_mode(0o600)).unwrap();
    let ca_path = ca_path.to_str().unwrap();
    json_success(
        dir,
        &["adapter", "trust", "register", "--ca-pem", ca_path, "--yes"],
        env,
    );
}

fn activate_local_ocr(dir: &TempDir, env: &[(&str, &str)]) {
    configure_local_ocr(dir, env);
    json_success(dir, &["init"], env);
    json_success(
        dir,
        &["adapter", "approve", "paddleocr_vl_local", "--yes"],
        env,
    );
}

fn tasks(dir: &TempDir) -> Vec<Value> {
    let path = dir.path().join(".kio").join("tasks.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn chunk_texts(dir: &TempDir) -> Vec<String> {
    let path = dir.path().join(".kio").join("index").join("chunks.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|row| row.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn ledger_charge_rows(dir: &TempDir) -> i64 {
    let mut total = 0;
    for candidate in [
        dir.path().join(".test-data").join("kio").join("ledger.db"),
        dir.path().join(".kio").join("ledger.db"),
    ] {
        if !candidate.exists() {
            continue;
        }
        let connection = Connection::open(&candidate).unwrap();
        let tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for table in tables
            .iter()
            .filter(|name| name.contains("charge") || name.contains("batch_request"))
        {
            total += connection
                .query_row::<i64, _, _>(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .unwrap();
        }
    }
    total
}

fn scanned_pdf_fixture() -> TempDir {
    let dir = canonical_tempdir();
    // No text layer, so the deterministic Prepare mints no units and the file
    // is enqueued for enrichment. That is the whole point: this is the shape a
    // local OCR pipeline exists to handle.
    fs::write(dir.path().join("scan.pdf"), "%PDF-1.4\nscanned page\n").unwrap();
    dir
}

/// A standalone image, which reaches the OCR route for a different reason.
///
/// A scanned PDF gets there by having no text layer; an image gets there by
/// being a recognized binary Prepare will not parse at all. Both end with no
/// prepared units, so both depend on the adapter discovering them -- and until
/// this fixture existed every test on this route was a PDF, so the discovery
/// code only ever had to be right about pages.
fn write_scan_png(dir: &TempDir) {
    // A complete 1×1 RGBA PNG. The mock ignores its pixels, but the native
    // image route must still fully decode the input before reaching OCR.
    let png: [u8; 68] = [
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0b, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0xf8,
        0x0f, 0x04, 0x00, 0x09, 0xfb, 0x03, 0xfd, 0xfb, 0x5e, 0x6b, 0x2b, 0x00, 0x00, 0x00, 0x00,
        b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];
    fs::write(dir.path().join("scan.png"), png).unwrap();
}

fn scanned_image_fixture() -> TempDir {
    let dir = canonical_tempdir();
    write_scan_png(&dir);
    dir
}

fn scanned_pdf_and_image_fixture() -> TempDir {
    let dir = scanned_pdf_fixture();
    write_scan_png(&dir);
    dir
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OcrNormalizeWitness {
    raw_hash: String,
    tool_profile_hash: String,
    generation: u64,
    manifest_hash: String,
}

fn current_ocr_normalizes(dir: &TempDir) -> BTreeMap<String, OcrNormalizeWitness> {
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.head_commit_hash().unwrap().unwrap();
    let commit = repo.read_commit(&head).unwrap();
    let tree = repo.read_tree(&commit.tree).unwrap();
    ["scan.pdf", "scan.png"]
        .into_iter()
        .filter_map(|path| {
            let entry = tree.entries.iter().find(|entry| entry.path == path)?;
            let normalize = entry.normalize.as_ref()?;
            Some((
                path.to_owned(),
                OcrNormalizeWitness {
                    raw_hash: entry.raw_hash.clone(),
                    tool_profile_hash: normalize.tool_profile_hash.clone(),
                    generation: normalize.r#gen,
                    manifest_hash: normalize.manifest_hash.clone(),
                },
            ))
        })
        .collect()
}

fn markdownize_task_count(dir: &TempDir, suffix: &str) -> usize {
    tasks(dir)
        .into_iter()
        .filter(|task| task["type"] == "markdownize")
        .filter(|task| {
            task["input_path"]
                .as_str()
                .is_some_and(|path| path.ends_with(suffix))
        })
        .count()
}

/// The same one-command flow, for an image instead of a PDF.
#[test]
fn s3e_one_index_enriches_a_standalone_image_through_the_local_pipeline() {
    let dir = scanned_image_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    let tasks = tasks(&dir);
    let scan = tasks
        .iter()
        .find(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.png"))
        })
        .unwrap_or_else(|| panic!("no task for scan.png: {tasks:?}"));
    assert_eq!(scan["status"], "done", "{scan}");
    assert_eq!(scan["fallback_reason"], "local_adapter_done", "{scan}");

    let texts = chunk_texts(&dir);
    assert!(
        texts.iter().any(|text| text.contains(MOCK_PAGE_TEXT)),
        "the local pipeline must have produced the image's body: {texts:?}"
    );

    // OCR must retain the original pixels alongside any extracted figures,
    // including after derived storage is reconstructed from the current HEAD.
    let search_args = ["search", "Kio local OCR mock", "--mode", "text"];
    let before = json_success(&dir, &search_args, &local);
    let images = before["results"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|result| result["related_images"].as_array().unwrap())
        .map(|image| image["image_uri"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let repo = kio_core::scope::Repository::open(dir.path()).unwrap();
    let raw = fs::read(dir.path().join("scan.png")).unwrap();
    let source_uri = format!(
        "kio://{}/object/image/{}",
        repo.scope_identity().unwrap().scope_id,
        kio_pipeline::prepare::hash_bytes(&raw)
    );
    assert_eq!(
        images.iter().filter(|uri| **uri == source_uri).count(),
        1,
        "{before}"
    );
    assert_eq!(
        images.len(),
        2,
        "the mock's extracted figure must remain: {before}"
    );
    json_success(&dir, &["open", &source_uri], &local);
    json_success(&dir, &["repair", "rebuild-db", "--offline"], &local);
    let rebuilt = json_success(&dir, &search_args, &local);
    let rebuilt_images = rebuilt["results"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|result| result["related_images"].as_array().unwrap())
        .map(|image| image["image_uri"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(rebuilt_images, images, "{rebuilt}");

    // A current policy change must hide both the derived row and its image
    // object immediately, without waiting for a new index operation.
    fs::write(dir.path().join(".kioignore"), "scan.png\n").unwrap();
    let denied = json_success(&dir, &search_args, &local);
    assert_eq!(denied["results"].as_array().unwrap().len(), 0, "{denied}");
    assert!(
        !kio(&dir, &["open", &source_uri], &local)
            .output()
            .unwrap()
            .status
            .success()
    );
}

/// One `kio index` is the whole flow.
///
/// The online lane splits enqueue from send because sending needs approval and
/// money. A local pipeline needs neither, so requiring a second command would
/// be ceremony guarding nothing — and the task is still created first, so a
/// crash mid-OCR leaves work the next index picks up.
#[test]
fn s3e_one_index_enriches_a_scanned_pdf_through_the_local_pipeline() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    let texts = chunk_texts(&dir);
    assert!(
        texts.iter().any(|text| text.contains(MOCK_PAGE_TEXT)),
        "the local pipeline must have produced the PDF's body: {texts:?}"
    );

    let tasks = tasks(&dir);
    let scan = tasks
        .iter()
        .find(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.pdf"))
        })
        .unwrap_or_else(|| panic!("no task for scan.pdf: {tasks:?}"));
    assert_eq!(scan["status"], "done", "{scan}");
    assert_eq!(scan["fallback_reason"], "local_adapter_done", "{scan}");
}

/// Explicit local-peer authorization plus the no-billing rule, checked from
/// outside the adapter.
#[test]
fn s3e_the_granted_local_route_opens_no_paid_ledger_row() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    assert!(
        chunk_texts(&dir)
            .iter()
            .any(|text| text.contains(MOCK_PAGE_TEXT)),
        "the explicitly granted local adapter must enrich the PDF"
    );
    assert_eq!(
        ledger_charge_rows(&dir),
        0,
        "a local pipeline has no invoice, so nothing may be reserved or settled"
    );
}

/// The task must never be visible to the online lane.
///
/// `output_ref`'s prefix is what every online gate matches on — the network
/// opt-in, the ledger reservation, the batch sender, the auth revive. A local
/// task keyed `online:` would be swept into all of them.
#[test]
fn s3e_the_local_task_is_not_addressed_to_the_online_lane() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    for task in tasks(&dir) {
        let output_ref = task["output_ref"].as_str().unwrap_or_default();
        assert!(
            !output_ref.starts_with("online:"),
            "a local enrichment must not be addressed to the online lane: {task}"
        );
    }
}

/// The figure the local pipeline extracted must be reachable from the search
/// result that cites it (05 §1.7).
///
/// This is the assertion that was missing when Stage 3 first met a real server.
/// PaddleOCR-VL writes figures as HTML `<img src="…">` and never `![](…)`, and
/// `kio-search`'s `extract_related_images` — which also decides which images get
/// embedded, what the scope projection counts, and what purge treats as an
/// orphan — reads only the CommonMark form. Every check that stopped at "the
/// normalized Markdown contains a `kio://` URI" passed anyway, because the URI
/// was there; it was simply written in a spelling nothing downstream could read.
/// So this asserts the field the contract actually promises, and then opens what
/// it names.
#[test]
fn s3e_the_local_pipelines_figure_is_reachable_from_the_chunk_that_cites_it() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    let search = json_success(&dir, &["search", "mock", "--mode", "text"], &local);
    let hit = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["result_type"] == "chunk")
        .unwrap_or_else(|| panic!("no chunk hit for the mock page: {search}"));
    let images = hit["related_images"]
        .as_array()
        .unwrap_or_else(|| panic!("the cited figure must be enumerated: {hit}"));
    assert_eq!(images.len(), 1, "{hit}");
    let uri = images[0]["image_uri"].as_str().unwrap();
    assert!(uri.contains("/object/image/sha256:"), "{uri}");

    let opened = json_success(&dir, &["open", uri], &local);
    assert_eq!(opened["status"], "opened", "{opened}");
    assert_eq!(opened["object_type"], "image", "{opened}");
}

/// `related_images[]` offers the figure and withholds the sticker.
///
/// On a real infographic six queries returned 54 references and two of them
/// were figures; the rest were decoration sitting next to text that already
/// said the thing. Every reference costs the Agent a `kio open`, so a list that
/// is 96% decoration is not a richer answer, it is a more expensive one.
///
/// The measurement is what has to be asserted here. A count would pass just as
/// well against a filter that dropped the last image, or the smallest-indexed
/// one, or every second one -- so this pins WHICH survives, by opening it and
/// reading back the bytes the figure was made of.
#[test]
fn s3e_a_chunks_related_images_offer_the_figure_and_not_the_decoration() {
    let dir = scanned_pdf_fixture();
    let local = [
        ("KIO_TEST_LOCAL_OCR", "mock"),
        ("KIO_TEST_LOCAL_OCR_BODY", "decorated"),
    ];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    let search = json_success(&dir, &["search", "mock", "--mode", "text"], &local);
    let hit = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["result_type"] == "chunk")
        .unwrap_or_else(|| panic!("no chunk hit for the mock page: {search}"));
    let images = hit["related_images"]
        .as_array()
        .unwrap_or_else(|| panic!("the cited figure must be enumerated: {hit}"));
    assert_eq!(images.len(), 1, "only the figure is worth opening: {hit}");
    assert_eq!(images[0]["order"], 0, "{hit}");

    // Which one survived. The two mock images differ only in their bytes, so
    // opening the survivor is the only way to tell the figure from the sticker.
    let uri = images[0]["image_uri"].as_str().unwrap();
    let opened = json_success(&dir, &["open", uri], &local);
    assert_eq!(opened["status"], "opened", "{opened}");
    let bytes = fs::read(opened["path"].as_str().unwrap()).unwrap();
    assert!(
        String::from_utf8_lossy(&bytes).contains("mock figure"),
        "the surviving image must be the figure, not the icon: {opened}"
    );

    // Both objects still exist -- this thins what is offered, not what is kept.
    // Filtering the extractor instead would have made the icon an orphan.
    let objects = fs::read_dir(dir.path().join(".kio/objects/image"))
        .unwrap()
        .count();
    assert_eq!(objects, 2, "the decoration must remain in the archive");
}

/// Setting the ratio to zero brings the decoration back.
///
/// The threshold is drawn from four captured pages, which is thin, so the point
/// of the knob is that a corpus it reads wrongly can be corrected without
/// re-indexing anything. That only holds if the knob is reachable: the schema
/// rejects unknown keys, so a default that works while the config key is
/// refused would be a filter nobody could turn off.
#[test]
fn s3e_the_related_image_floor_can_be_lowered_without_reindexing() {
    let dir = scanned_pdf_fixture();
    let local = [
        ("KIO_TEST_LOCAL_OCR", "mock"),
        ("KIO_TEST_LOCAL_OCR_BODY", "decorated"),
    ];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    // Written after indexing on purpose: nothing was discarded when the page
    // was archived, so this is a pure read-side change.
    let config = dir.path().join(".kio/config.toml");
    fs::write(&config, "[search]\nrelated_images_min_area_ratio = 0.0\n").unwrap();

    // `--scope` because this key layers like every other `[search]` key
    // (PC49/PC50): a folder value counts only for a single, non-`--descendants`
    // scope, and the default multi-scope search reads the device layer alone.
    // A search without it would keep the default floor and pass this test only
    // if the assertion were the other way round.
    let scope = dir.path().to_str().unwrap();
    let search = json_success(
        &dir,
        &["search", "mock", "--mode", "text", "--scope", scope],
        &local,
    );
    let hit = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["result_type"] == "chunk")
        .unwrap();
    let images = hit["related_images"].as_array().unwrap();
    assert_eq!(images.len(), 2, "the floor must be lowerable: {hit}");
}

/// A page whose content is a table gets indexed, and its cells are searchable.
///
/// This one is about the size of the hole, not the mechanism. Two of the three
/// documents the GPU box ever captured hold a table, and both of them indexed
/// *nothing* — not a degraded reading, no reading. The unit tests over the real
/// captures show the notation is right; this shows a table's text actually
/// reaches the index and comes back out of `kio search`, which is the only claim
/// that matters to whoever asked why their invoice was not findable.
#[test]
fn s3e_a_page_whose_content_is_a_table_is_indexed_and_searchable() {
    let dir = scanned_pdf_fixture();
    let local = [
        ("KIO_TEST_LOCAL_OCR", "mock"),
        ("KIO_TEST_LOCAL_OCR_BODY", "table"),
    ];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    let tasks = tasks(&dir);
    let scan = tasks
        .iter()
        .find(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.pdf"))
        })
        .unwrap_or_else(|| panic!("no task for scan.pdf: {tasks:?}"));
    assert_eq!(scan["status"], "done", "{scan}");

    // `Handwritten board` exists only inside a cell, so a hit on it cannot come
    // from anywhere but the converted table.
    let search = json_success(&dir, &["search", "Handwritten", "--mode", "text"], &local);
    let hit = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["result_type"] == "chunk")
        .unwrap_or_else(|| panic!("a table cell must be searchable: {search}"));
    // And the figure that sat inside a cell is offered like any other.
    let images = hit["related_images"]
        .as_array()
        .unwrap_or_else(|| panic!("the cell's figure must be enumerated: {hit}"));
    assert_eq!(images.len(), 1, "{hit}");
    let opened = json_success(
        &dir,
        &["open", images[0]["image_uri"].as_str().unwrap()],
        &local,
    );
    assert_eq!(opened["status"], "opened", "{opened}");
}

/// A unit that fails 07 §5's acceptance check must not reach the archive.
///
/// The check has always detected raw HTML; what it did not do was stop anything.
/// Its single production caller used the result only to decide whether to take
/// the Done shortcut, and the count-based status returns Done for "1 unit
/// produced, 0 failed" regardless — so the local route wrote raw `<div>` into
/// normalized units for a release with nothing saying so. 07 §9 then freezes
/// whatever landed.
///
/// The offline route now refuses instead. It can afford to: nothing was billed
/// and re-running is free, which is not true of the online routes and is why
/// they are deliberately left as they were.
#[test]
fn s3e_a_unit_that_fails_the_v1_acceptance_check_is_refused_not_frozen() {
    let dir = scanned_pdf_fixture();
    let local = [
        ("KIO_TEST_LOCAL_OCR", "mock"),
        // A table with a merged cell. Plain tables are converted now, so the
        // refusal has to be exercised by something GFM genuinely cannot write —
        // otherwise this test would keep passing while testing nothing.
        ("KIO_TEST_LOCAL_OCR_BODY", "nonconforming"),
    ];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);

    let tasks = tasks(&dir);
    let scan = tasks
        .iter()
        .find(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.pdf"))
        })
        .unwrap_or_else(|| panic!("no task for scan.pdf: {tasks:?}"));
    assert_eq!(scan["status"], "failed", "{scan}");
    assert_eq!(scan["fallback_reason"], "contract_violation", "{scan}");

    // The refusal is only worth anything if nothing was persisted. A unit that
    // reached the index would be frozen there by 07 §9's first-instance-wins.
    assert!(
        !chunk_texts(&dir)
            .iter()
            .any(|text| text.contains(MOCK_PAGE_TEXT)),
        "a refused unit must not be indexed: {:?}",
        chunk_texts(&dir)
    );
}

/// Without the backend, nothing about the online route changes.
#[test]
fn s3e_an_undeclared_local_backend_leaves_the_online_route_alone() {
    let dir = scanned_pdf_fixture();
    json_success(&dir, &["init"], &[]);
    json_success(&dir, &["index", "--offline"], &[]);

    let tasks = tasks(&dir);
    let scan = tasks
        .iter()
        .find(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.pdf"))
        })
        .unwrap_or_else(|| panic!("no task for scan.pdf: {tasks:?}"));
    assert_eq!(
        scan["output_ref"], "online:mistral_ocr_markdownize",
        "{scan}"
    );
    assert_eq!(
        scan["status"], "paused",
        "offline must block the HTTP task: {scan}"
    );
    assert!(
        !chunk_texts(&dir)
            .iter()
            .any(|text| text.contains(MOCK_PAGE_TEXT)),
        "no local backend is declared, so its text must not appear"
    );
}

/// `--offline` is an egress stop, including authenticated loopback HTTPS.
///
/// The local adapter is still a separate process and receives the complete
/// document body, so it must remain pending without constructing its client
/// when the caller requested no HTTP at all.
#[test]
fn s3e_offline_blocks_the_granted_local_https_peer() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index", "--offline"], &local);

    let tasks = tasks(&dir);
    let scan = tasks
        .iter()
        .find(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.pdf"))
        })
        .unwrap_or_else(|| panic!("no task for scan.pdf: {tasks:?}"));
    assert_eq!(scan["output_ref"], "offline:paddleocr_vl_local", "{scan}");
    assert_eq!(scan["status"], "pending", "{scan}");
    assert!(
        !chunk_texts(&dir)
            .iter()
            .any(|text| text.contains(MOCK_PAGE_TEXT)),
        "--offline must not dispatch the local HTTPS peer"
    );
}

/// A second index over unchanged content must not re-run the pipeline.
///
/// The task reaching `done` is what stops it. Were it left Pending, every
/// subsequent `kio index` would re-OCR the whole document — free in money, but
/// minutes of GPU each time.
#[test]
fn s3e_a_second_index_does_not_re_run_the_finished_pipeline() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);
    let after_first = tasks(&dir).len();

    json_success(&dir, &["index"], &local);
    let scan_tasks: Vec<_> = tasks(&dir)
        .into_iter()
        .filter(|task| {
            task.get("input_path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("scan.pdf"))
        })
        .collect();
    assert!(
        scan_tasks.iter().all(|task| task["status"] != "pending"),
        "a finished document must not be re-enqueued: {scan_tasks:?}"
    );
    assert!(
        tasks(&dir).len() >= after_first,
        "the task journal is append-only"
    );
}

/// Enabling the unrelated embedding lane must not recompute a current,
/// complete local-OCR normalized instance. Keep the local OCR selector on its
/// exact mock profile but poison its body: if either the PDF or PNG reaches the
/// peer again, the acceptance check would fail and expose the resend.
#[test]
fn s3e_embedding_enablement_reuses_current_pdf_and_png_ocr_instances() {
    let dir = scanned_pdf_and_image_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);
    let before = current_ocr_normalizes(&dir);
    assert_eq!(
        before.len(),
        2,
        "both OCR inputs need HEAD refs: {before:?}"
    );
    let pdf_tasks_before = markdownize_task_count(&dir, "scan.pdf");
    let png_tasks_before = markdownize_task_count(&dir, "scan.png");

    // The paid mock only changes embedding configuration and its explicit
    // grant. It neither changes the local OCR profile nor talks to a provider.
    let with_embedding = [
        ("KIO_TEST_LOCAL_OCR", "mock"),
        ("KIO_TEST_LOCAL_OCR_BODY", "nonconforming"),
        ("KIO_TEST_GEMINI_EMBED", "mock"),
    ];
    json_success(&dir, &["ledger", "init"], &with_embedding);
    json_success(
        &dir,
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
        &with_embedding,
    );
    json_success(&dir, &["index", "--online"], &with_embedding);

    assert_eq!(
        current_ocr_normalizes(&dir),
        before,
        "embedding enablement must retain exact HEAD OCR refs and their pinned manifests"
    );
    assert_eq!(markdownize_task_count(&dir, "scan.pdf"), pdf_tasks_before);
    assert_eq!(markdownize_task_count(&dir, "scan.png"), png_tasks_before);
}

/// Reuse is content-addressed: changing bytes must create a new OCR task and
/// may never borrow the predecessor's pinned normalize ref.
#[test]
fn s3e_changed_raw_bytes_do_not_reuse_a_current_ocr_normalize_ref() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);
    let before = current_ocr_normalizes(&dir);
    let task_count = markdownize_task_count(&dir, "scan.pdf");

    fs::write(
        dir.path().join("scan.pdf"),
        "%PDF-1.4\nscanned page changed after OCR\n",
    )
    .unwrap();
    json_success(&dir, &["index"], &local);

    let after = current_ocr_normalizes(&dir);
    assert_ne!(
        after, before,
        "changed raw bytes must not retain the old OCR ref"
    );
    assert!(
        markdownize_task_count(&dir, "scan.pdf") > task_count,
        "changed raw bytes must enqueue a fresh OCR task"
    );
}

/// Switching from the mock profile to the configured real local profile is a
/// provenance change. `--offline` prevents a peer request while proving the
/// old mock ref is not reused under the real profile selection.
#[test]
fn s3e_changed_ocr_profile_does_not_borrow_a_mock_normalize_ref() {
    let dir = scanned_pdf_fixture();
    let local = [("KIO_TEST_LOCAL_OCR", "mock")];
    activate_local_ocr(&dir, &local);
    json_success(&dir, &["index"], &local);
    let before = current_ocr_normalizes(&dir);
    let task_count = markdownize_task_count(&dir, "scan.pdf");

    json_success(&dir, &["index", "--offline"], &[]);

    assert_ne!(
        current_ocr_normalizes(&dir),
        before,
        "the real selected profile must not retain a mock OCR normalize ref"
    );
    assert!(
        markdownize_task_count(&dir, "scan.pdf") > task_count,
        "a changed OCR profile must enqueue work instead of borrowing the mock result"
    );
}
