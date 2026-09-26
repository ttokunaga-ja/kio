//! Step4b Office conversion layer + QB41 contract tests.
//!
//! Source: docs/07-adapter-spec.md §5.1 ("Office intermediate の変換機構
//! (DOCX / PPTX — 実装フィードバック 2026-07-23)"), docs/04-pipeline.md §2
//! (unit table) / §5.3 (contract_violation semantics), and QB41
//! (tasks/step4b-contract-tests-p3b.md L762-775, normative source
//! docs/03-data-model.md §2.1's "prepare profile / renderer 変更による
//! prepared_hash 変化が駆動する再 Markdownize" second legal gen+1 path).
//!
//! Mirrors the harness conventions of `step4b_p3a_contract.rs` /
//! `step3_p0_contract.rs`: the `kio()` runner (per-`Command` env, never
//! process-global mutation), `fake_pdf`, tolerant-of-any-exit seam runners,
//! and manifest/unit inspection by reading the CAS store directly.
//!
//! The Office conversion seam (`kio_adapter::office_convert`):
//! `KIO_TEST_OFFICE_CONVERT` names a fixture PDF file whose bytes are
//! returned VERBATIM for ANY input by `OfficeConverter::convert_to_pdf` —
//! the office input files below are deliberately arbitrary small bytes named
//! `*.docx` / `*.pptx` (the seam ignores their content entirely).

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use kio_adapter::office_convert::OFFICE_CONVERTER_ENV;
use serde_json::Value;
use tempfile::TempDir;

const TEST_OFFICE_CONVERT_ENV: &str = "KIO_TEST_OFFICE_CONVERT";
const TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV: &str = "KIO_TEST_MISTRAL_OCR";

const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_HOLD_LOCK_READY",
    "KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID",
    "KIO_TEST_SCOPE_SEARCH_DELAY_MS",
    "KIO_TEST_R13_2_AUTH",
    "KIO_TEST_R13_2_DECLARED",
    "KIO_TEST_R13_2_FALLBACK",
    "KIO_TEST_WINDOWS_PROFILE",
    "KIO_TEST_OFFICE_CONVERT",
    "KIO_OFFICE_CONVERTER",
    "KIO_TEST_CAPTURE_SENT_MEDIA",
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

/// A minimal fake PDF with one text-bearing page per string (mirrors
/// `step3_p0_contract.rs` / `step4b_p3a_contract.rs`'s helper of the same
/// name/shape). Used here as the office converter SEAM's fixture target —
/// `KIO_TEST_OFFICE_CONVERT` returns this file's bytes verbatim for ANY
/// input, standing in for "the converted PDF" a real `soffice` would
/// otherwise produce from a DOCX/PPTX.
fn fake_pdf(pages: &[&str]) -> String {
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", index + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "%PDF-1.4\n1 0 obj << /Type /Pages /Kids [{kids}] /Count {} >> endobj\n",
        pages.len()
    );
    for (index, page) in pages.iter().enumerate() {
        out.push_str(&format!(
            "{} 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT ({page}) Tj ET\nendstream endobj\n",
            index + 2
        ));
    }
    out.push_str("%%EOF\n");
    out
}

fn write_office_fixture_pdf(dir: &TempDir, name: &str, pages: &[&str]) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, fake_pdf(pages)).unwrap();
    path
}

/// The seam ignores input content entirely — arbitrary bytes stand in for a
/// real `.docx`/`.pptx` file.
fn write_office_input(dir: &TempDir, name: &str) {
    fs::write(
        dir.path().join(name),
        b"not a real office file; the KIO_TEST_OFFICE_CONVERT seam ignores this content",
    )
    .unwrap();
}

// ---- normalized-instance inspection (mirrors step3_p0_contract.rs) --------

/// Every identity-leaf directory (one per `(raw_hash, tool_profile_hash, gen)`
/// normalized instance currently on disk) under
/// `.kio/objects/normalized_units`, found by walking the `ab/cd/` CAS fanout.
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
            if is_gen_dir_name(name) {
                found.push(path);
            } else {
                stack.push(path);
            }
        }
    }
    found
}

/// Whether `name` ends in a canonical `.g<digits>` generation suffix (e.g.
/// `"<raw64>.<tool64>.g0"`) — the leaf directory shape, as opposed to an `ab`/
/// `cd` fanout directory two levels up.
fn is_gen_dir_name(name: &str) -> bool {
    match name.rfind(".g") {
        Some(pos) => {
            let suffix = &name[pos + 2..];
            !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
        }
        None => false,
    }
}

fn gen_dir_names_under(units_root: &Path) -> Vec<String> {
    gen_dirs_under(units_root)
        .into_iter()
        .filter_map(|path| path.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .collect()
}

fn gen_dir_with_suffix(units_root: &Path, suffix: &str) -> PathBuf {
    gen_dirs_under(units_root)
        .into_iter()
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        })
        .unwrap_or_else(|| {
            panic!(
                "no gen dir ending in {suffix} under {}",
                units_root.display()
            )
        })
}

fn manifest_json(gen_dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(gen_dir.join("manifest.json")).unwrap()).unwrap()
}

/// The `type=markdownize` task(s) for `input_path` from a `kio status --json`
/// response's `tasks` array.
fn markdownize_tasks_for<'a>(status: &'a Value, input_path: &str) -> Vec<&'a Value> {
    status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == "markdownize" && task["input_path"] == input_path)
        .collect()
}

/// The completed ONLINE markdownize task for `input_path` (mirrors
/// `step4b_p3a_contract.rs`'s `online_markdownize_task`: distinguished by
/// `output_ref` still the `"online:"` placeholder, OR by
/// `fallback_reason="online_adapter_done"` once executed).
fn online_markdownize_task_for<'a>(status: &'a Value, input_path: &str) -> Option<&'a Value> {
    markdownize_tasks_for(status, input_path)
        .into_iter()
        .find(|task| {
            task["output_ref"]
                .as_str()
                .is_some_and(|output_ref| output_ref.starts_with("online:"))
                || task["fallback_reason"] == "online_adapter_done"
        })
}

// ===========================================================================
// office_01 — DOCX offline baseline: page:N units, searchable, no online send
// ===========================================================================

#[test]
fn office_01_docx_offline_pages_searchable() {
    let dir = tempfile::tempdir().unwrap();
    // Fixtures live OUTSIDE the scope root so `kio index` never scans/indexes
    // the fixture PDF itself as a second, unrelated document.
    let fixtures = tempfile::tempdir().unwrap();
    let fixture = write_office_fixture_pdf(
        &fixtures,
        "fixture_a.pdf",
        &["office01 unique searchable phrase kilogram"],
    );
    write_office_input(&dir, "report.docx");
    init(&dir);

    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture.display().to_string())
        .arg("--json")
        .assert()
        .success();

    // Offline page:N unit exists in the persisted manifest.
    let units_root = dir.path().join(".kio/objects/normalized_units");
    let gen_dirs = gen_dirs_under(&units_root);
    assert_eq!(
        gen_dirs.len(),
        1,
        "exactly one normalized instance (the offline baseline) should exist: {gen_dirs:?}"
    );
    let manifest = manifest_json(&gen_dirs[0]);
    let units = manifest["units"].as_array().unwrap();
    assert!(
        units
            .iter()
            .any(|unit| unit["unit_key"] == "page:1" && unit["status"] == "done"),
        "expected a done page:1 unit: {manifest}"
    );

    // The phrase is searchable offline.
    let search = kio(&dir, &["search", "kilogram", "--mode", "text"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&search).unwrap();
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "expected a search hit for the offline-baseline text: {search}"
    );
    assert!(
        search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["title"] == "report.docx"),
        "{search}"
    );

    // No online send happened: an ENQUEUED-but-Pending "ready for enhancement"
    // online task is expected (the converter resolved, so item 1's enqueue
    // gate does not hold it back) — what must NOT have happened is an actual
    // SEND. `fallback_reason="online_adapter_done"` is the one marker
    // `execute_online_markdownize_task` stamps on a task it actually executed
    // (docs comment on `enqueue_online_placeholder_task`'s idempotency check).
    let status = json_success(&dir, &["status"]);
    let markdownize_tasks = markdownize_tasks_for(&status, "report.docx");
    assert!(!markdownize_tasks.is_empty(), "{status}");
    assert!(
        markdownize_tasks
            .iter()
            .all(|task| task["fallback_reason"] != "online_adapter_done"),
        "no online send must have happened: {status}"
    );
    let online_task = markdownize_tasks
        .iter()
        .find(|task| {
            task["output_ref"]
                .as_str()
                .is_some_and(|output_ref| output_ref.starts_with("online:"))
        })
        .unwrap_or_else(|| panic!("expected an enqueued (Pending) online task: {status}"));
    assert_eq!((*online_task)["status"], "paused", "{status}");
    assert_eq!(
        (*online_task)["hold_reason"],
        "ledger_initialization_required",
        "{status}"
    );
    assert!(
        !dir.path()
            .join(".test-data/kio/cost-ledger.sqlite")
            .exists()
    );
}

// ===========================================================================
// office_02 — PPTX offline baseline: slide:1..N units, searchable offline
// ===========================================================================

#[test]
fn office_02_pptx_slide_units_offline() {
    let dir = tempfile::tempdir().unwrap();
    // Fixtures live OUTSIDE the scope root — see office_01's comment.
    let fixtures = tempfile::tempdir().unwrap();
    let fixture = write_office_fixture_pdf(
        &fixtures,
        "fixture_b.pdf",
        &[
            "office02 first slide unique text",
            "office02 second slide unique text",
            "office02 third slide unique text",
        ],
    );
    write_office_input(&dir, "deck.pptx");
    init(&dir);

    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture.display().to_string())
        .arg("--json")
        .assert()
        .success();

    let units_root = dir.path().join(".kio/objects/normalized_units");
    let gen_dirs = gen_dirs_under(&units_root);
    assert_eq!(gen_dirs.len(), 1, "{gen_dirs:?}");
    let manifest = manifest_json(&gen_dirs[0]);
    let unit_keys: Vec<String> = manifest["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|unit| unit["unit_key"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        unit_keys,
        vec!["slide:1", "slide:2", "slide:3"],
        "slide units visible in the manifest: {manifest}"
    );
    for unit in manifest["units"].as_array().unwrap() {
        assert_eq!(unit["unit_type"], "slide", "{manifest}");
        assert_eq!(unit["status"], "done", "{manifest}");
    }

    // Slide text is searchable offline.
    let search = kio(&dir, &["search", "office02", "--mode", "text"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&search).unwrap();
    let results = search["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{search}");
    assert!(
        results.iter().any(|result| result["title"] == "deck.pptx"),
        "{search}"
    );
}

// ===========================================================================
// office_03 — online send uses the CONVERTED pdf (media_type + %PDF magic)
// ===========================================================================

#[test]
fn office_03_online_send_uses_converted_pdf() {
    let dir = tempfile::tempdir().unwrap();
    // Fixtures live OUTSIDE the scope root — see office_01's comment.
    let fixtures = tempfile::tempdir().unwrap();
    let fixture =
        write_office_fixture_pdf(&fixtures, "fixture_c.pdf", &["office03 online send text"]);
    write_office_input(&dir, "report.docx");
    init(&dir);
    kio(&dir, &["ledger", "init"]).assert().success();

    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture.display().to_string())
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .arg("--json")
        .assert()
        .success();
    kio(&dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();

    let status = json_success(&dir, &["status"]);
    assert!(
        !markdownize_tasks_for(&status, "report.docx").is_empty(),
        "an online markdownize task must have been enqueued: {status}"
    );

    let capture_path = dir.path().join("captured-sent-media.txt");
    kio(&dir, &["batch", "resume"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture.display().to_string())
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .env(
            "KIO_TEST_CAPTURE_SENT_MEDIA",
            capture_path.display().to_string(),
        )
        .arg("--json")
        .assert()
        .success();

    let status = json_success(&dir, &["status"]);
    let task = online_markdownize_task_for(&status, "report.docx")
        .unwrap_or_else(|| panic!("no completed online markdownize task: {status}"));
    assert!(
        task["status"] == "done" || task["status"] == "partial",
        "{status}"
    );

    // The mock OCR client boundary observed application/pdf bytes carrying
    // the %PDF magic — the CONVERTED pdf, never the original (fake) docx
    // bytes, crossed into the (mocked) OCR client.
    let captured = fs::read_to_string(&capture_path)
        .unwrap_or_else(|err| panic!("capture file was not written: {err}"));
    let mut lines = captured.lines();
    assert_eq!(
        lines.next(),
        Some("application/pdf"),
        "sent media_type must be application/pdf: {captured}"
    );
    assert_eq!(
        lines.next(),
        Some("true"),
        "sent bytes must carry the %PDF magic: {captured}"
    );
}

// ===========================================================================
// office_04 — converter absent: no doomed task; recovers once resolvable
// ===========================================================================

#[test]
fn office_04_converter_absent_no_doomed_task() {
    let dir = tempfile::tempdir().unwrap();
    // Fixtures live OUTSIDE the scope root — see office_01's comment.
    let fixtures = tempfile::tempdir().unwrap();
    write_office_input(&dir, "report.docx");
    init(&dir);

    // Seam unset + explicit override unset + PATH scrubbed (per-Command env,
    // not process-global mutation) — no converter resolves, even on a
    // machine with a real soffice on PATH.
    kio(&dir, &["index", "--yes"])
        .env_remove(TEST_OFFICE_CONVERT_ENV)
        .env_remove(OFFICE_CONVERTER_ENV)
        .env("PATH", "/nonexistent-kio-test-path-office04")
        .arg("--json")
        .assert()
        .success();

    // No markdownize task at all for report.docx (no doomed online task, and
    // no offline baseline either — item 2's "no crash, simply unenriched").
    let status = json_success(&dir, &["status"]);
    assert!(
        markdownize_tasks_for(&status, "report.docx").is_empty(),
        "no markdownize task must exist while the converter is unavailable: {status}"
    );

    // index_status carries office_conversion_unavailable with the file.
    let search = kio(&dir, &["search", "anything", "--mode", "text"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search: Value = serde_json::from_slice(&search).unwrap();
    let unavailable = &search["index_status"]["office_conversion_unavailable"];
    assert_eq!(unavailable["count"], 1, "{search}");
    let files = unavailable["files"].as_array().unwrap();
    assert!(
        files.iter().any(|file| file["path"] == "report.docx"),
        "{search}"
    );

    // Set the seam and re-index: enqueue happens (idempotent recovery).
    let fixture = write_office_fixture_pdf(
        &fixtures,
        "fixture_d.pdf",
        &["office04 recovered searchable text"],
    );
    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture.display().to_string())
        .arg("--json")
        .assert()
        .success();

    let status = json_success(&dir, &["status"]);
    assert!(
        !markdownize_tasks_for(&status, "report.docx").is_empty(),
        "a markdownize task must exist once a converter resolves: {status}"
    );

    let search2 = kio(&dir, &["search", "anything", "--mode", "text"])
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search2: Value = serde_json::from_slice(&search2).unwrap();
    assert_eq!(
        search2["index_status"]["office_conversion_unavailable"]["count"], 0,
        "the disposition must clear once the converter resolves and the file is \
         re-indexed: {search2}"
    );
}

// ===========================================================================
// office_05 — QB41: prepared_hash drift (renderer change) confirms, then
// creates gen+1 while preserving gen 0 (first-instance-wins)
// ===========================================================================

#[test]
fn office_05_qb41_renderer_drift_prompts_then_gen1() {
    let dir = tempfile::tempdir().unwrap();
    // Fixtures live OUTSIDE the scope root — see office_01's comment.
    let fixtures = tempfile::tempdir().unwrap();
    let fixture_a = write_office_fixture_pdf(
        &fixtures,
        "fixture_a.pdf",
        &["office05 renderer version ALPHA distinct text"],
    );
    write_office_input(&dir, "report.docx");
    init(&dir);

    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture_a.display().to_string())
        .arg("--json")
        .assert()
        .success();

    let units_root = dir.path().join(".kio/objects/normalized_units");
    assert_eq!(
        gen_dir_names_under(&units_root).len(),
        1,
        "exactly one (g0) instance after the first index"
    );

    // Switch the seam to fixture B (a different renderer output — different
    // text, changing every unit's prepared_hash under the SAME raw_hash).
    let fixture_b = write_office_fixture_pdf(
        &fixtures,
        "fixture_b.pdf",
        &["office05 renderer version BETA completely different wording"],
    );

    // Non-interactive `kio index` (no --yes): refused.
    let stderr = kio(&dir, &["index"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture_b.display().to_string())
        .arg("--json")
        .assert()
        .code(9)
        .get_output()
        .stderr
        .clone();
    let error: Value = serde_json::from_slice(&stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-CONFIRM-REJECTED-001", "{error}");

    // NO new instance was created by the refused attempt.
    let gens_after_refusal = gen_dir_names_under(&units_root);
    assert_eq!(
        gens_after_refusal.len(),
        1,
        "a refused confirmation must not create a new generation: {gens_after_refusal:?}"
    );
    assert!(
        gens_after_refusal[0].ends_with(".g0"),
        "{gens_after_refusal:?}"
    );

    // Re-run with the yes-flag: new gen instance exists (gen advanced), old
    // preserved (first-instance-wins).
    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture_b.display().to_string())
        .arg("--json")
        .assert()
        .success();

    let gens_after_confirm = gen_dir_names_under(&units_root);
    assert_eq!(
        gens_after_confirm.len(),
        2,
        "gen 0 must be preserved AND a new gen created: {gens_after_confirm:?}"
    );
    assert!(
        gens_after_confirm.iter().any(|name| name.ends_with(".g0")),
        "{gens_after_confirm:?}"
    );
    assert!(
        gens_after_confirm.iter().any(|name| name.ends_with(".g1")),
        "{gens_after_confirm:?}"
    );

    // The new generation's manifest declares gen=1, parent_gen=0, and reflects
    // fixture B's content (a different prepared_hash than gen 0's).
    let gen0_dir = gen_dir_with_suffix(&units_root, ".g0");
    let gen1_dir = gen_dir_with_suffix(&units_root, ".g1");
    let gen0_manifest = manifest_json(&gen0_dir);
    let gen1_manifest = manifest_json(&gen1_dir);
    assert_eq!(gen1_manifest["gen"], 1, "{gen1_manifest}");
    assert_eq!(gen1_manifest["parent_gen"], 0, "{gen1_manifest}");
    assert_eq!(gen0_manifest["gen"], 0, "gen 0 untouched: {gen0_manifest}");
    assert_ne!(
        gen0_manifest["units"][0]["prepared_hash"], gen1_manifest["units"][0]["prepared_hash"],
        "the renderer drift must be reflected in a changed prepared_hash: \
         gen0={gen0_manifest} gen1={gen1_manifest}"
    );
}

// ===========================================================================
// office_06 — runtime conversion failure joins contract_violation semantics
// ===========================================================================

#[test]
fn office_06_conversion_failure_contract_violation() {
    let dir = tempfile::tempdir().unwrap();
    // Fixtures live OUTSIDE the scope root — see office_01's comment.
    let fixtures = tempfile::tempdir().unwrap();
    let fixture =
        write_office_fixture_pdf(&fixtures, "fixture_e.pdf", &["office06 initial ok text"]);
    write_office_input(&dir, "report.docx");
    init(&dir);
    kio(&dir, &["ledger", "init"]).assert().success();

    kio(&dir, &["index", "--yes"])
        .env(TEST_OFFICE_CONVERT_ENV, fixture.display().to_string())
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .arg("--json")
        .assert()
        .success();
    kio(&dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();
    let status = json_success(&dir, &["status"]);
    assert!(
        !markdownize_tasks_for(&status, "report.docx").is_empty(),
        "an online markdownize task must have been enqueued: {status}"
    );

    // Point the seam at a NONEXISTENT fixture path. `convert_to_pdf`'s file
    // read fails with `AdapterError::ContractViolation` (verified against
    // `kio_adapter::office_convert::OfficeConverter::convert_to_pdf`'s Seam
    // backend, which maps a missing fixture to that error rather than
    // panicking), joining contract_violation semantics (04 §5.3: retryable).
    let missing = fixtures.path().join("does-not-exist.pdf");
    let _ = kio(&dir, &["batch", "resume"])
        .env(TEST_OFFICE_CONVERT_ENV, missing.display().to_string())
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .arg("--json")
        .assert();

    let status = json_success(&dir, &["status"]);
    let task = markdownize_tasks_for(&status, "report.docx")
        .into_iter()
        .find(|task| {
            task["output_ref"]
                .as_str()
                .is_some_and(|output_ref| output_ref.starts_with("online:"))
        })
        .unwrap_or_else(|| panic!("no online markdownize task remained: {status}"));
    assert_eq!(task["status"], "failed", "{status}");
    assert_eq!(task["fallback_reason"], "contract_violation", "{status}");
    assert!(
        task["attempts"].as_u64().unwrap() >= 1,
        "attempts must advance per 04 §5.3: {status}"
    );

    // errors.jsonl carries a record of the failure (redacted/generic per the
    // existing failure plumbing, not necessarily the raw adapter string —
    // see the implementation report).
    let errors =
        fs::read_to_string(dir.path().join(".test-data/kio/logs/errors.jsonl")).unwrap_or_default();
    assert!(
        !errors.trim().is_empty(),
        "the conversion failure must reach errors.jsonl"
    );
}

// ===========================================================================
// office_07 — a scanned (garbage-gated, empty-prepare) PDF with a DONE online
// (OCR) instance re-indexes quietly: no drift verdict, no gen churn
// ===========================================================================

/// QB41 non-firing pin (03 §2.1 / 07 §5.1): the local extractor yields ZERO
/// prepared units for a no-text-layer PDF (R20-4 garbage gate — equally true
/// of every real-world FlateDecode-compressed PDF, including real soffice
/// output), while its done ONLINE instance discovered its units from the OCR
/// response. "Empty fresh prepare" vs "OCR-discovered manifest" must never
/// read as renderer drift (exit-9 churn on every re-index; an `--yes`
/// offline gen+1 from unreadable input would shadow the OCR content). Today
/// TWO independent layers keep this true — run_index_pipeline's R20-5
/// empty-prepare arm peels the candidate off before the done-instance branch,
/// and `prepared_hash_drift_new_gen`'s guards (a)/(b) refuse the verdict at
/// the mint (revert-testing showed the R20-5 arm alone already passes this
/// test; the guards are the backstop for a future reachability change, e.g. a
/// FlateDecode-capable extractor) — this test pins the end-to-end behavior
/// whichever layer is doing the work.
#[test]
fn office_07_scanned_pdf_done_instance_is_not_drift() {
    let dir = tempfile::tempdir().unwrap();
    // PDF magic, no `BT` text operator anywhere: prepare extracts nothing
    // real and garbage-gates to an empty unit set, routing to online OCR.
    fs::write(
        dir.path().join("scan.pdf"),
        b"%PDF-1.4\n1 0 obj << /Type /Pages >> endobj\n\x01\x02\x03\x7f\x00binary-image-noise\n%%EOF\n",
    )
    .unwrap();
    init(&dir);
    kio(&dir, &["ledger", "init"]).assert().success();

    kio(&dir, &["index", "--yes"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .arg("--json")
        .assert()
        .success();
    kio(&dir, &["adapter", "approve", "--all", "--yes"])
        .assert()
        .success();
    kio(&dir, &["batch", "resume"])
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock")
        .arg("--json")
        .assert()
        .success();

    let status = json_success(&dir, &["status"]);
    let task = online_markdownize_task_for(&status, "scan.pdf")
        .unwrap_or_else(|| panic!("no completed online markdownize task: {status}"));
    assert!(
        task["status"] == "done" || task["status"] == "partial",
        "{status}"
    );
    let units_root = dir.path().join(".kio/objects/normalized_units");
    let gens = gen_dir_names_under(&units_root);
    assert_eq!(gens.len(), 1, "exactly one (g0) OCR instance: {gens:?}");

    // The dangerous re-index: NO --yes. Must succeed (an empty fresh prepare
    // is not a drift verdict) and must NOT create a second generation.
    kio(&dir, &["index"]).arg("--json").assert().success();
    let gens = gen_dir_names_under(&units_root);
    assert_eq!(
        gens.len(),
        1,
        "an empty fresh prepare must never drive a gen+1: {gens:?}"
    );
    assert!(gens[0].ends_with(".g0"), "{gens:?}");
}
