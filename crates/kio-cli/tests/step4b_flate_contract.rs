//! FlateDecode offline-extraction contract tests (07 §2.1, 2026-07-23
//! addendum).
//!
//! Real-world text-layer PDFs (TeX / LibreOffice output) carry FlateDecode-
//! compressed content streams whose glyphs are subset-font indices mapped
//! through per-font ToUnicode CMaps. These tests pin the end-to-end CLI
//! behavior the `kio_adapter::pdf_decode` graph decoder introduces:
//!
//! - flate_01: a LibreOffice-shaped PDF (1-byte codes, `bfchar` CMap,
//!   FlateDecode content stream) indexes OFFLINE — page:1 unit, searchable —
//!   while the R21-4 online-enhancement placeholder still coexists (same
//!   model as an uncompressed text-layer PDF).
//! - flate_02: a TeX-Live-shaped PDF whose Page and Font dictionaries live
//!   inside a compressed `/Type /ObjStm` container, with 2-byte
//!   Identity-H-style codes, decodes and searches the same way.
//!
//! Harness conventions mirror `step4b_office_contract.rs` (per-Command env,
//! CAS-store inspection, `kio status --json` task assertions).

mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use miniz_oxide::deflate::compress_to_vec_zlib;
use serde_json::Value;
use tempfile::TempDir;

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

fn json_success(dir: &TempDir, args: &[&str]) -> Value {
    let stdout = kio(dir, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).unwrap()
}

fn init(dir: &TempDir) {
    kio(dir, &["init"]).assert().success();
}

fn zlib(data: &[u8]) -> Vec<u8> {
    compress_to_vec_zlib(data, 6)
}

fn obj(number: u32, body: &[u8]) -> Vec<u8> {
    let mut out = format!("{number} 0 obj\n").into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b"\nendobj\n");
    out
}

fn stream_obj(number: u32, dict: &str, payload: &[u8]) -> Vec<u8> {
    let mut body = dict.as_bytes().to_vec();
    body.extend_from_slice(b"\nstream\n");
    body.extend_from_slice(payload);
    body.extend_from_slice(b"\nendstream");
    obj(number, &body)
}

/// Hex of a string's UTF-16BE code units, for `bfchar` destination tokens.
fn utf16_hex(text: &str) -> String {
    text.encode_utf16()
        .map(|unit| format!("{unit:04X}"))
        .collect()
}

/// LibreOffice-shaped fixture: FlateDecode content stream showing 1-byte
/// glyph codes; the bfchar CMap maps each code to one WORD, and the -400
/// kerns become word gaps.
fn one_byte_flate_pdf(words: [&str; 3]) -> Vec<u8> {
    let content = zlib(b"BT\n/F1 10.5 Tf\n[<01>-400<02>-400<03>]TJ\nET\n");
    let cmap_text = format!(
        "begincmap\n1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
         3 beginbfchar\n<01> <{}>\n<02> <{}>\n<03> <{}>\nendbfchar\nendcmap",
        utf16_hex(words[0]),
        utf16_hex(words[1]),
        utf16_hex(words[2]),
    );
    let cmap = zlib(cmap_text.as_bytes());
    let mut pdf = b"%PDF-1.6\n".to_vec();
    pdf.extend(obj(
        1,
        b"<< /Type /Page /Contents 2 0 R /Resources << /Font << /F1 3 0 R >> >> >>",
    ));
    pdf.extend(stream_obj(
        2,
        &format!("<< /Length {} /Filter /FlateDecode >>", content.len()),
        &content,
    ));
    pdf.extend(obj(
        3,
        b"<< /Type /Font /Subtype /Type0 /ToUnicode 4 0 R >>",
    ));
    pdf.extend(stream_obj(
        4,
        &format!("<< /Length {} /Filter /FlateDecode >>", cmap.len()),
        &cmap,
    ));
    pdf.extend_from_slice(b"%%EOF\n");
    pdf
}

/// TeX-Live-shaped fixture: the Page and Font dictionaries are packed inside
/// a compressed `/Type /ObjStm`; the content stream shows 2-byte codes.
fn objstm_two_byte_flate_pdf(words: [&str; 2]) -> Vec<u8> {
    let content = zlib(b"BT\n/F7 9.9 Tf\n[<0102>-375<0103>]TJ\nET\n");
    let cmap_text = format!(
        "begincmap\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
         2 beginbfchar\n<0102> <{}>\n<0103> <{}>\nendbfchar\nendcmap",
        utf16_hex(words[0]),
        utf16_hex(words[1]),
    );
    let cmap = zlib(cmap_text.as_bytes());
    let member_page = b"<< /Type /Page /Contents 5 0 R /Resources << /Font << /F7 3 0 R >> >> >>";
    let member_font = b"<< /Type /Font /Encoding /Identity-H /ToUnicode 6 0 R >>";
    let header = format!("2 0 3 {} ", member_page.len() + 1);
    let first = header.len();
    let mut objstm_data = header.into_bytes();
    objstm_data.extend_from_slice(member_page);
    objstm_data.push(b'\n');
    objstm_data.extend_from_slice(member_font);
    let objstm = zlib(&objstm_data);

    let mut pdf = b"%PDF-1.6\n".to_vec();
    pdf.extend(stream_obj(
        1,
        &format!(
            "<< /Type /ObjStm /N 2 /First {first} /Length {} /Filter /FlateDecode >>",
            objstm.len()
        ),
        &objstm,
    ));
    pdf.extend(stream_obj(
        5,
        &format!("<< /Length {} /Filter /FlateDecode >>", content.len()),
        &content,
    ));
    pdf.extend(stream_obj(
        6,
        &format!("<< /Length {} /Filter /FlateDecode >>", cmap.len()),
        &cmap,
    ));
    pdf.extend_from_slice(b"%%EOF\n");
    pdf
}

// ---- CAS-store inspection (mirrors step4b_office_contract.rs) --------------

fn gen_dirs_under(units_root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![units_root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_gen = match name.rfind(".g") {
                Some(pos) => {
                    let suffix = &name[pos + 2..];
                    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
                }
                None => false,
            };
            if is_gen {
                found.push(path);
            } else {
                stack.push(path);
            }
        }
    }
    found
}

fn manifest_json(gen_dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(gen_dir.join("manifest.json")).unwrap()).unwrap()
}

fn online_markdownize_task_for<'a>(status: &'a Value, input_path: &str) -> Option<&'a Value> {
    status["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|task| task["type"] == "markdownize" && task["input_path"] == input_path)
        .find(|task| {
            task["output_ref"]
                .as_str()
                .is_some_and(|output_ref| output_ref.starts_with("online:"))
                || task["fallback_reason"] == "online_adapter_done"
        })
}

fn assert_search_hit(dir: &TempDir, needle: &str, title: &str) {
    let search = kio(dir, &["search", needle, "--mode", "text"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&search).unwrap();
    let results = search["results"].as_array().unwrap();
    assert!(
        results.iter().any(|result| result["title"] == title),
        "expected `{needle}` to hit {title}: {search}"
    );
}

// ===========================================================================
// flate_01 — compressed 1-byte-CMap PDF: offline page unit + search, online
// enhancement placeholder coexists (same model as uncompressed text-layer)
// ===========================================================================

#[test]
fn flate_01_compressed_cid_pdf_indexes_offline_with_enhancement_pending() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("report.pdf"),
        one_byte_flate_pdf(["flateround", "evidence", "ledger"]),
    )
    .unwrap();
    init(&dir);
    kio(&dir, &["index", "--yes"])
        .arg("--json")
        .assert()
        .success();
    kio(&dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();

    // Offline baseline: one gen-0 instance whose page:1 unit is done.
    let units_root = dir.path().join(".kio/objects/normalized_units");
    let gen_dirs = gen_dirs_under(&units_root);
    assert_eq!(gen_dirs.len(), 1, "{gen_dirs:?}");
    let manifest = manifest_json(&gen_dirs[0]);
    let units = manifest["units"].as_array().unwrap();
    assert_eq!(units.len(), 1, "{manifest}");
    assert_eq!(units[0]["unit_key"], "page:1", "{manifest}");
    assert_eq!(units[0]["status"], "done", "{manifest}");

    // Decoded CMap words are searchable offline.
    assert_search_hit(&dir, "flateround", "report.pdf");
    assert_search_hit(&dir, "evidence", "report.pdf");

    // R21-4 coexistence: the online enhancement placeholder is still
    // enqueued for a text-layer PDF; FlateDecode support must not silently
    // drop the enrichment lane.
    let status = json_success(&dir, &["status"]);
    let online_task = online_markdownize_task_for(&status, "report.pdf")
        .unwrap_or_else(|| panic!("online enhancement task missing: {status}"));
    assert_eq!(online_task["status"], "paused", "{status}");
    assert_eq!(
        online_task["hold_reason"], "ledger_initialization_required",
        "{status}"
    );
    assert!(
        !dir.path()
            .join(".test-data/kio/cost-ledger.sqlite")
            .exists()
    );
}

// ===========================================================================
// flate_03 — a raster PDF whose COMPRESSED bytes contain a chance "BT" (and
// whose raw bytes carry printable `(...)` metadata literals) must index as an
// empty-prepare OCR placeholder — never fail the command with the
// write_prepared_objects page/hash cardinality error (2026-07-23 fixture
// registration regression: real img2pdf rasters hit exactly this).
// ===========================================================================

#[test]
fn flate_03_chance_bt_raster_pdf_routes_to_ocr_without_schema_error() {
    let dir = canonical_tempdir();
    // Structure mirrors an img2pdf raster: one /Type /Page whose content
    // stream draws an image XObject; the "pixel" stream is raw binary noise
    // (NOT valid deflate) that embeds a bare "BT" so the raw text-layer scan
    // chance-hits; document-info literals are printable and would lossy-scan
    // into a garbage "page" if the write path re-extracted.
    let mut noise = vec![0x91_u8, 0x02, 0x7f, 0x33];
    noise.extend_from_slice(b"BT");
    noise.extend(vec![0x8e_u8; 64]);
    let mut pdf = b"%PDF-1.4\n".to_vec();
    pdf.extend(obj(1, b"<< /Count 1 /Kids [ 2 0 R ] /Type /Pages >>"));
    pdf.extend(obj(
        2,
        b"<< /Type /Page /Parent 1 0 R /Contents 3 0 R /Resources << /XObject << /Im0 4 0 R >> >> >>",
    ));
    pdf.extend(stream_obj(
        3,
        "<< /Length 28 >>",
        b"q 100 0 0 100 0 0 cm /Im0 Do Q",
    ));
    pdf.extend(stream_obj(
        4,
        &format!(
            "<< /Subtype /Image /Width 8 /Height 8 /Filter /FlateDecode /Length {} >>",
            noise.len()
        ),
        &noise,
    ));
    pdf.extend(obj(
        5,
        b"<< /CreationDate (D:20260723082757Z) /Producer (img2pdf 0.6.1) >>",
    ));
    pdf.extend_from_slice(b"%%EOF\n");
    fs::write(dir.path().join("scan-like.pdf"), pdf).unwrap();
    init(&dir);

    kio(&dir, &["index", "--yes"])
        .arg("--json")
        .assert()
        .success();
    kio(&dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();

    // Empty prepare → no offline instance; the online enhancement
    // placeholder owns the file (R20-5).
    let units_root = dir.path().join(".kio/objects/normalized_units");
    assert!(gen_dirs_under(&units_root).is_empty());
    let status = json_success(&dir, &["status"]);
    let online_task = online_markdownize_task_for(&status, "scan-like.pdf")
        .unwrap_or_else(|| panic!("OCR placeholder missing: {status}"));
    assert_eq!(online_task["status"], "paused", "{status}");
    assert_eq!(
        online_task["hold_reason"], "ledger_initialization_required",
        "{status}"
    );
    assert!(
        !dir.path()
            .join(".test-data/kio/cost-ledger.sqlite")
            .exists()
    );
}

// ===========================================================================
// flate_02 — ObjStm-packed dictionaries + 2-byte codes decode and search
// ===========================================================================

#[test]
fn flate_02_objstm_two_byte_codes_index_offline() {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("paper.pdf"),
        objstm_two_byte_flate_pdf(["twobyte", "harvest"]),
    )
    .unwrap();
    init(&dir);
    kio(&dir, &["index", "--yes"])
        .arg("--json")
        .assert()
        .success();
    kio(&dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();

    let units_root = dir.path().join(".kio/objects/normalized_units");
    let gen_dirs = gen_dirs_under(&units_root);
    assert_eq!(gen_dirs.len(), 1, "{gen_dirs:?}");
    let manifest = manifest_json(&gen_dirs[0]);
    assert_eq!(
        manifest["units"].as_array().unwrap()[0]["unit_key"],
        "page:1",
        "{manifest}"
    );

    assert_search_hit(&dir, "twobyte", "paper.pdf");
    assert_search_hit(&dir, "harvest", "paper.pdf");
}
