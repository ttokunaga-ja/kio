//! Prepare stage contracts.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization as _;

use crate::{IoResultExt, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitType {
    Page,
    Slide,
    HeadingSection,
    Sheet,
    Image,
    File,
    Symbol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitFingerprint {
    pub perceptual_hash: String,
    pub text_hash: String,
    pub visual_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedUnit {
    pub order: u64,
    pub unit_key: String,
    pub unit_type: UnitType,
    pub prepared_hash: String,
    /// Identity of every preparation component whose output can affect reuse.
    /// This is deliberately separate from `prepared_hash`, which remains a
    /// content address for the prepared bytes alone.
    pub preparation_profile_hash: String,
    pub fingerprint: UnitFingerprint,
    pub mime: Option<String>,
    pub page_number: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareStageRequest {
    pub raw_hash: String,
    pub media_type: String,
    pub input_path: String,
    pub tool_profile_hash: String,
}

/// Prepare from a caller-verified byte buffer. `input_path` is display/provenance
/// metadata only and is never reopened by this API.
#[derive(Debug, Clone, Copy)]
pub struct PrepareStageBytesRequest<'a> {
    pub raw_hash: &'a str,
    pub media_type: &'a str,
    pub input_path: &'a str,
    pub tool_profile_hash: &'a str,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareStageOutput {
    pub prepared_object_hashes: Vec<String>,
    pub prepared_units: Vec<PreparedUnit>,
    pub image_object_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitMapping {
    pub unchanged: Vec<UnitReuse>,
    pub changed_unit_keys: Vec<String>,
    pub added_unit_keys: Vec<String>,
    pub removed_unit_keys: Vec<String>,
    pub change_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitReuse {
    pub old_unit_key: String,
    pub new_unit_key: String,
    pub confidence: String,
    pub reason: String,
}

/// Maximum number of dynamic-programming cells used for exact unit alignment.
/// At 8 bytes per `usize`, this caps cell storage at about 16 MiB before row
/// overhead. Larger comparisons take the deterministic full-change fallback.
pub const MAX_UNIT_LCS_CELLS: usize = 2_000_000;

pub fn prepare_units(request: PrepareStageRequest) -> Result<PrepareStageOutput> {
    // R20-6: the local deterministic adapter only parses text (Markdown / plain text /
    // code) and PDF text layers (docs/07 §7.1). A RECOGNIZED non-text-native binary (image
    // / OOXML office document) is never locally parseable — a raw-bytes "passthrough" unit
    // evidences its bytes (a ZIP's PK header, an image's pixels) as searchable garbage.
    // Route it to online OCR (empty prepared_units → the R20-5 path). `octet-stream`
    // (unrecognized extension) is content-sniffed below: it may be a TEXT file with an
    // unknown extension (`.env`, `.cfg`) — still passthrough'd locally — or a binary blob.
    let media_type = request.media_type.as_str();
    let is_text_native = matches!(media_type, "text/markdown" | "text/plain" | "text/x-code");
    let is_pdf = media_type == "application/pdf";
    if !is_text_native && !is_pdf && media_type != "application/octet-stream" {
        return Ok(empty_prepare_output());
    }
    let bytes = std::fs::read(&request.input_path).pipeline_io(Path::new(&request.input_path))?;
    prepare_units_from_bytes(PrepareStageBytesRequest {
        raw_hash: &request.raw_hash,
        media_type: &request.media_type,
        input_path: &request.input_path,
        tool_profile_hash: &request.tool_profile_hash,
        bytes: &bytes,
    })
}

/// Prepare logical units from exactly the byte buffer whose identity the caller
/// supplied. This closes path-reopen races between raw hashing and preparation.
pub fn prepare_units_from_bytes(
    request: PrepareStageBytesRequest<'_>,
) -> Result<PrepareStageOutput> {
    let media_type = request.media_type;
    let is_text_native = matches!(media_type, "text/markdown" | "text/plain" | "text/x-code");
    let is_pdf = media_type == "application/pdf";
    // 07 §5.1 (2026-07-23 addendum): DOCX/PPTX route through the Office
    // converter below instead of the octet-stream/text gates — they are
    // recognized binary formats with their own unit-ization path, not a
    // passthrough or an OCR-only dead end.
    let is_office = kio_adapter::office_convert::is_office_media(media_type);
    // 07 §5.1 (2026-07-25 ruling): XLSX does NOT go through the converter. A
    // sheet has no visual unit — rendering one to PDF paginates it by print
    // area and cuts a wide table down the middle, leaving the halves of every
    // row in different units with nothing to rejoin them by. It is extracted
    // directly instead, below.
    let is_xlsx = kio_adapter::xlsx_extract::is_xlsx_media(media_type);
    if !is_text_native
        && !is_pdf
        && !is_office
        && !is_xlsx
        && media_type != "application/octet-stream"
    {
        return Ok(empty_prepare_output());
    }
    let bytes = request.bytes;
    let prepared_hash = hash_bytes(bytes);
    if prepared_hash != request.raw_hash {
        return Err(crate::PipelineError::contract(
            "KIO-E-PREPARE-IDENTITY-001",
            format!(
                "prepare bytes do not match the declared raw hash for {}",
                request.input_path
            ),
        ));
    }
    if !is_text_native && !is_pdf && !is_office && !is_xlsx && !bytes_are_text(bytes) {
        // Binary octet-stream (an unrecognized-extension binary blob): do NOT evidence its
        // raw bytes as text — route to OCR like the other non-text-native media above.
        // (Office media is binary too, but never reaches this text-passthrough check either
        // way — the `is_office` exemption above skips it in favor of the conversion path.)
        return Ok(empty_prepare_output());
    }
    let unit_type = unit_type_for_media_type(request.media_type);
    // The declared prepare profile is part of reuse identity for every input.
    // Office inputs additionally bind the resolved renderer identity below.
    let mut preparation_profile_hash = request.tool_profile_hash.to_owned();
    // 07 §5.1 (2026-07-25 ruling): one unit per worksheet, keyed by the sheet's
    // own name (QB27), with the unit's hash and fingerprint taken from the
    // extracted Markdown — the same shape the PDF branch uses for page text, so
    // an edit inside one sheet reprepares that sheet alone.
    if is_xlsx {
        let document = kio_adapter::xlsx_extract::extract_xlsx(bytes).map_err(|err| {
            crate::PipelineError::contract("KIO-E-PREPARE-XLSX-EXTRACT-001", err.to_string())
        })?;
        let unit_keys = sheet_unit_keys(
            &document
                .sheets
                .iter()
                .map(|sheet| sheet.name.clone())
                .collect::<Vec<_>>(),
        );
        let mut prepared_units = Vec::with_capacity(document.sheets.len());
        for (index, sheet) in document.sheets.iter().enumerate() {
            let unit_key = unit_keys[index].clone();
            let markdown = sheet.markdown.as_bytes();
            prepared_units.push(PreparedUnit {
                order: index as u64,
                unit_key,
                unit_type: UnitType::Sheet,
                prepared_hash: hash_bytes(markdown),
                preparation_profile_hash: preparation_profile_hash.clone(),
                fingerprint: fingerprint_for_bytes(markdown, markdown),
                mime: Some(request.media_type.to_owned()),
                page_number: None,
            });
        }
        return Ok(PrepareStageOutput {
            prepared_object_hashes: prepared_units
                .iter()
                .map(|unit| unit.prepared_hash.clone())
                .collect(),
            prepared_units,
            // An embedded chart is genuinely visual and direct extraction
            // cannot read it. Routing those to the image → OCR lane needs an
            // evidence identity for "image N inside sheet M" that does not
            // exist yet, so this stays empty rather than half-wired — the
            // extractor counts them so the gap is visible (backlog §7.2 I9).
            image_object_hashes: Vec::new(),
        });
    }
    // 07 §5.1 (2026-07-23 addendum): DOCX/PPTX unit-ize via a converted-PDF
    // intermediate produced by an external renderer (LibreOffice `soffice`).
    // No renderer in this environment → stay silent/crash-free — the exact
    // same empty shape as the R20-4 scanned-PDF route below (the CLI layer
    // gates online enqueue/visibility separately via `index_status`, 05
    // §1.7 — prepare itself must never enqueue a doomed task, nor crash).
    // A runtime conversion failure surfaces as a prepare-stage contract
    // error (07 §5.1: "実行時の変換失敗は contract_violation に合流する" — any
    // KIO-E-PREPARE-* code the task-retry classifier does not specifically
    // recognize falls through to the `ContractViolation` bucket already,
    // `kio_pipeline::task::task_retry_kind`'s catch-all arm).
    let office_converted_pdf = if is_office {
        match kio_adapter::office_convert::resolve_office_converter() {
            None => return Ok(empty_prepare_output()),
            Some(converter) => {
                preparation_profile_hash = office_preparation_profile_hash(
                    request.tool_profile_hash,
                    &converter.profile_identity(),
                );
                Some(converter.convert_to_pdf(bytes, media_type).map_err(|err| {
                    crate::PipelineError::contract(
                        "KIO-E-PREPARE-OFFICE-CONVERT-001",
                        err.to_string(),
                    )
                })?)
            }
        }
    } else {
        None
    };
    // Page AND Slide are both "paginated" unit types once Office conversion
    // is in the mix (previously only Page was — Slide's unit_count was
    // hardcoded to 1, which was moot before this change because PPTX never
    // reached this far: the very first gate above always returned empty for
    // it). `pdf_pages` below is populated for either a native PDF or a
    // successfully-converted Office document; per-unit hashes/fingerprints
    // downstream key off `paginated`, not `unit_type == UnitType::Page`.
    let paginated = matches!(unit_type, UnitType::Page | UnitType::Slide);
    let pdf_pages = if is_pdf {
        let pages = pdf_text_pages_bounded(bytes)?;
        // R20-4: `pdf_has_text_layer` and the stream/literal extractors match on raw
        // (undecompressed) bytes, so a scanned PDF's compressed image streams yield garbage
        // "text" (a chance `BT` plus random `(...)` literals). If every recovered page is
        // empty or mostly non-printable, there is no real text layer — treat it as scanned
        // and route to OCR rather than persisting the garbage as a searchable unit. (An
        // empty `pages` — the honest no-text-layer result — also lands here vacuously.)
        if pages.iter().all(|page| !is_probably_real_text(page)) {
            return Ok(empty_prepare_output());
        }
        // R21-5: for a MIXED PDF (a real text page + a scanned image page) the per-page
        // suppression of a scanned page's garbage is applied in the deterministic
        // markdownize adapter (`read_pdf_page_text`), which is what actually produces the
        // page markdown / chunk text — `prepare_units` only builds the unit skeleton.
        pages
    } else if let Some(converted_pdf) = office_converted_pdf.as_deref() {
        let pages = pdf_text_pages_bounded(converted_pdf)?;
        // Same R20-4 garbage-suppression gate as the native-PDF branch above,
        // applied to the CONVERTED PDF's text layer instead of the raw
        // office bytes.
        if pages.iter().all(|page| !is_probably_real_text(page)) {
            return Ok(empty_prepare_output());
        }
        pages
    } else {
        Vec::new()
    };
    let unit_count = if paginated { pdf_pages.len().max(1) } else { 1 };
    let mut prepared_units = Vec::new();
    for index in 0..unit_count {
        let selector = match unit_type {
            UnitType::Page | UnitType::Slide => (index + 1).to_string(),
            UnitType::Sheet => "Sheet1".to_owned(),
            UnitType::Image => index.to_string(),
            UnitType::File | UnitType::HeadingSection | UnitType::Symbol => "1".to_owned(),
        };
        let unit_key = canonical_unit_key(unit_type, &selector);
        let page_bytes = pdf_pages
            .get(index)
            .map(|page| page.as_bytes())
            .unwrap_or(bytes);
        // `paginated` (Page OR Slide) hashes/fingerprints off the PAGE bytes —
        // for is_pdf that is the native PDF's own page text; for is_office it is
        // the CONVERTED page text, so renderer drift changes prepared_hash (03
        // §2.1's prepare-profile/renderer-driven gen+1 path is what absorbs
        // that). mime stays the ORIGINAL office media type either way (below).
        let unit_prepared_hash = if paginated {
            hash_bytes(page_bytes)
        } else if unit_count == 1 {
            prepared_hash.clone()
        } else {
            hash_bytes(format!("{prepared_hash}\0{unit_key}").as_bytes())
        };
        let fingerprint = if paginated {
            fingerprint_for_bytes(page_bytes, page_bytes)
        } else {
            fingerprint_for_bytes(bytes, unit_prepared_hash.as_bytes())
        };
        prepared_units.push(PreparedUnit {
            order: index as u64,
            unit_key,
            unit_type,
            prepared_hash: unit_prepared_hash,
            preparation_profile_hash: preparation_profile_hash.clone(),
            fingerprint,
            mime: Some(request.media_type.to_owned()),
            // Only Page carries page_number, unchanged from before this
            // change (Slide never did either, for the same PPTX-via-`_`
            // reason DOCX now no longer falls into). Kept that way here too:
            // `page_number`'s only downstream reader is the PDF-page-image
            // path (07 §5.1's `metadata: unit_kind, page_number, ...`,
            // scoped to `page` unit_kind), and 04 §2's unit table gives
            // Slide its own `slide:N` identity — a redundant page_number
            // would just restate the `slide:N` ordinal under a PDF-specific
            // name.
            page_number: (unit_type == UnitType::Page).then_some(index as u64 + 1),
        });
    }
    Ok(PrepareStageOutput {
        prepared_object_hashes: prepared_units
            .iter()
            .map(|unit| unit.prepared_hash.clone())
            .collect(),
        prepared_units,
        image_object_hashes: Vec::new(),
    })
}

fn office_preparation_profile_hash(request_profile_hash: &str, converter_profile: &str) -> String {
    // Delimit and domain-separate the two opaque identities so neither string
    // can be substituted for the other or create an ambiguous concatenation.
    hash_bytes(
        format!("kio-office-preparation-profile-v1\0{request_profile_hash}\0{converter_profile}")
            .as_bytes(),
    )
}

fn empty_prepare_output() -> PrepareStageOutput {
    PrepareStageOutput {
        prepared_object_hashes: Vec::new(),
        prepared_units: Vec::new(),
        image_object_hashes: Vec::new(),
    }
}

#[must_use]
pub fn unit_ref(unit_key: &str) -> String {
    let digest = Sha256::digest(unit_key.as_bytes());
    lower_hex(&digest)[..16].to_owned()
}

#[must_use]
pub fn canonical_unit_key(unit_type: UnitType, selector: &str) -> String {
    match unit_type {
        UnitType::Page => format!("page:{}", selector.trim_start_matches('0').max("1")),
        UnitType::Slide => format!("slide:{}", selector.trim_start_matches('0').max("1")),
        // QB27 (04 §2 L125-128): NFC-normalize, then escape a literal `#` in
        // the sheet's own name to `##`. The caller appends `#2`, `#3` to the
        // second and later sheets sharing a name; escaping first is what keeps
        // the two uses of `#` from colliding — a real sheet named `A#2` becomes
        // `sheet:A##2` and cannot be read as the second sheet named `A`
        // (`sheet:A#2`).
        UnitType::Sheet => format!(
            "sheet:{}",
            selector.nfc().collect::<String>().replace('#', "##")
        ),
        UnitType::Image => format!("image:{selector}"),
        UnitType::File | UnitType::HeadingSection | UnitType::Symbol => "doc:1".to_owned(),
    }
}

/// QB27 (04 §2 L125-128): the `sheet:` unit key for each worksheet, in workbook
/// order.
///
/// Two transforms, and the order between them is the whole point. Each name is
/// NFC-normalized and has its own `#` escaped to `##` first; only then does the
/// 2nd and later sheet sharing a name take `#2`, `#3`. Suffixing first would
/// make a real sheet named `A#2` collide with the second sheet named `A`.
#[must_use]
pub fn sheet_unit_keys(names: &[String]) -> Vec<String> {
    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    names
        .iter()
        .map(|name| {
            let base = canonical_unit_key(UnitType::Sheet, name);
            let occurrence = seen.entry(base.clone()).or_insert(0);
            *occurrence += 1;
            if *occurrence == 1 {
                base
            } else {
                format!("{base}#{occurrence}")
            }
        })
        .collect()
}

#[must_use]
pub fn fingerprint_for_bytes(text_layer_bytes: &[u8], prepared_bytes: &[u8]) -> UnitFingerprint {
    let text_hash = hash_bytes(text_layer_bytes);
    let perceptual_hash = hash_bytes(prepared_bytes);
    UnitFingerprint {
        perceptual_hash: perceptual_hash.clone(),
        text_hash,
        visual_hash: perceptual_hash,
    }
}

#[must_use]
pub fn change_rate(changed: usize, added: usize, removed: usize, new_unit_count: usize) -> f64 {
    (changed + added + removed) as f64 / std::cmp::max(new_unit_count, 1) as f64
}

#[must_use]
pub fn map_units(old_units: &[PreparedUnit], new_units: &[PreparedUnit]) -> UnitMapping {
    if !unit_mapping_within_budget(old_units.len(), new_units.len()) {
        return full_changed_mapping(old_units, new_units);
    }
    let pairs = lcs_fingerprint_pairs(old_units, new_units);
    let mut unchanged = Vec::new();
    let mut changed_unit_keys = Vec::new();
    let mut added_unit_keys = Vec::new();
    let mut removed_unit_keys = Vec::new();
    let mut old_cursor = 0;
    let mut new_cursor = 0;

    for (old_anchor, new_anchor) in pairs
        .iter()
        .copied()
        .chain(std::iter::once((old_units.len(), new_units.len())))
    {
        align_changed_interval(
            &old_units[old_cursor..old_anchor],
            &new_units[new_cursor..new_anchor],
            &mut changed_unit_keys,
            &mut added_unit_keys,
            &mut removed_unit_keys,
        );
        if old_anchor < old_units.len() && new_anchor < new_units.len() {
            unchanged.push(UnitReuse {
                old_unit_key: old_units[old_anchor].unit_key.clone(),
                new_unit_key: new_units[new_anchor].unit_key.clone(),
                confidence: "1.0".to_owned(),
                reason: "fingerprint_exact".to_owned(),
            });
        }
        old_cursor = old_anchor.saturating_add(1);
        new_cursor = new_anchor.saturating_add(1);
    }

    UnitMapping {
        change_rate: change_rate(
            changed_unit_keys.len(),
            added_unit_keys.len(),
            removed_unit_keys.len(),
            new_units.len(),
        ),
        unchanged,
        changed_unit_keys,
        added_unit_keys,
        removed_unit_keys,
    }
}

#[must_use]
pub fn unit_mapping_within_budget(old_len: usize, new_len: usize) -> bool {
    old_len
        .checked_add(1)
        .and_then(|old| new_len.checked_add(1).and_then(|new| old.checked_mul(new)))
        .is_some_and(|cells| cells <= MAX_UNIT_LCS_CELLS)
}

fn full_changed_mapping(old_units: &[PreparedUnit], new_units: &[PreparedUnit]) -> UnitMapping {
    let mut changed_unit_keys = Vec::new();
    let mut added_unit_keys = Vec::new();
    let mut removed_unit_keys = Vec::new();
    align_changed_interval(
        old_units,
        new_units,
        &mut changed_unit_keys,
        &mut added_unit_keys,
        &mut removed_unit_keys,
    );
    UnitMapping {
        change_rate: change_rate(
            changed_unit_keys.len(),
            added_unit_keys.len(),
            removed_unit_keys.len(),
            new_units.len(),
        ),
        unchanged: Vec::new(),
        changed_unit_keys,
        added_unit_keys,
        removed_unit_keys,
    }
}

#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", lower_hex(&digest))
}

/// Compute the same content hash as [`hash_bytes`] with fixed working memory.
pub fn hash_reader<R: Read>(mut reader: R) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", lower_hex(&digest.finalize())))
}

fn unit_type_for_media_type(media_type: &str) -> UnitType {
    match media_type {
        "application/pdf" => UnitType::Page,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" => UnitType::Image,
        // 07 §5.1 (2026-07-23 addendum): DOCX unit-izes as `page` via a
        // converted-PDF intermediate (04 §2's unit table), not `File`.
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => UnitType::Page,
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => UnitType::Sheet,
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            UnitType::Slide
        }
        _ => UnitType::File,
    }
}

fn pdf_has_text_layer(bytes: &[u8]) -> bool {
    !bytes.starts_with(b"%PDF")
        || bytes.windows(2).any(|window| window == b"BT")
        // 07 §2.1 (2026-07-23 FlateDecode addendum): a compressed text-layer
        // PDF (TeX / LibreOffice output) has no literal `BT` in its raw
        // bytes — the probe inflates FlateDecode streams (bounded) and asks
        // the graph decoder whether they carry real, ToUnicode-mappable
        // text. Scanned/image-only PDFs stay `false` and keep OCR routing.
        || kio_adapter::pdf_decode::pdf_compressed_text_probe(bytes)
}

/// R20-4: heuristic for whether extracted PDF text is REAL text vs binary garbage
/// lossy-decoded from a scanned PDF's compressed streams. Real text is overwhelmingly
/// printable; garbage is dense with U+FFFD replacement characters and control bytes.
/// Requires a strong printable majority (non-control-or-whitespace, non-replacement) over
/// a non-empty length. Legitimate text extracted from real `(text)` literals passes; the
/// random-`(...)` / whole-binary fallback of a scanned page does not.
fn is_probably_real_text(text: &str) -> bool {
    let trimmed = text.trim();
    let total = trimmed.chars().count();
    if total == 0 {
        return false;
    }
    let printable = trimmed
        .chars()
        .filter(|ch| *ch != '\u{fffd}' && (!ch.is_control() || ch.is_whitespace()))
        .count();
    printable * 100 >= total * 85
}

/// R20-6: whether an `application/octet-stream` (unrecognized-extension) file is TEXT that
/// the local adapter should passthrough (a `.env` / `.cfg` / extensionless text file),
/// versus a binary blob (a DOCX ZIP, a compiled artifact) that must route to OCR rather
/// than have its raw bytes evidenced as searchable text. Text decodes as UTF-8, has no NUL
/// byte, and is overwhelmingly printable.
fn bytes_are_text(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return false;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => is_probably_real_text(text),
        Err(_) => false,
    }
}

pub fn pdf_text_pages_bounded(bytes: &[u8]) -> Result<Vec<String>> {
    if !bytes.starts_with(b"%PDF") {
        return Ok(vec![String::from_utf8_lossy(bytes).into_owned()]);
    }
    if !pdf_has_text_layer(bytes) {
        return Ok(Vec::new());
    }
    kio_adapter::deterministic::extract_pdf_text_pages_bounded(
        bytes,
        kio_adapter::deterministic::MAX_DETERMINISTIC_PDF_PAGES,
    )
    .map_err(|err| crate::PipelineError::contract("KIO-E-PREPARE-PDF-LIMIT-001", err.to_string()))
}

fn align_changed_interval(
    old_units: &[PreparedUnit],
    new_units: &[PreparedUnit],
    changed_unit_keys: &mut Vec<String>,
    added_unit_keys: &mut Vec<String>,
    removed_unit_keys: &mut Vec<String>,
) {
    let paired = std::cmp::min(old_units.len(), new_units.len());
    for unit in &new_units[..paired] {
        changed_unit_keys.push(unit.unit_key.clone());
    }
    for unit in &new_units[paired..] {
        added_unit_keys.push(unit.unit_key.clone());
    }
    for unit in &old_units[paired..] {
        removed_unit_keys.push(unit.unit_key.clone());
    }
}

fn lcs_fingerprint_pairs(
    old_units: &[PreparedUnit],
    new_units: &[PreparedUnit],
) -> Vec<(usize, usize)> {
    let m = old_units.len();
    let n = new_units.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if reusable_unit_identity_matches(&old_units[i], &new_units[j]) {
                1 + dp[i + 1][j + 1]
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if reusable_unit_identity_matches(&old_units[i], &new_units[j]) {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

fn reusable_unit_identity_matches(old_unit: &PreparedUnit, new_unit: &PreparedUnit) -> bool {
    old_unit.preparation_profile_hash == new_unit.preparation_profile_hash
        && old_unit.fingerprint == new_unit.fingerprint
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared_page(order: u64, key: &str, fp: &str) -> PreparedUnit {
        PreparedUnit {
            order,
            unit_key: key.to_owned(),
            unit_type: UnitType::Page,
            prepared_hash: format!("sha256:{fp:0<64}"),
            preparation_profile_hash: "sha256:test-prepare-profile".to_owned(),
            fingerprint: UnitFingerprint {
                perceptual_hash: fp.to_owned(),
                text_hash: fp.to_owned(),
                visual_hash: fp.to_owned(),
            },
            mime: None,
            page_number: Some(order + 1),
        }
    }

    #[test]
    fn placeholder_unit_type_uses_snake_case() {
        let value = serde_json::to_value(UnitType::HeadingSection).expect("serialize unit type");
        assert_eq!(value, "heading_section");
    }

    #[test]
    fn unit_ref_vectors_match_step2a() {
        assert_eq!(unit_ref("page:12"), "3c2fa650872d5484");
        assert_eq!(unit_ref("page:1"), "00f081779b832543");
        assert_eq!(unit_ref("page:57"), "d2255263b6d52dc8");
        assert_eq!(unit_ref("slide:3"), "22814b0d608d29b9");
        assert_eq!(unit_ref("sheet:Sheet1"), "fae07767a7986381");
        assert_eq!(unit_ref("image:0"), "beadc43287ae0d1a");
    }

    #[test]
    fn change_rate_vectors_match_step2a() {
        assert!((change_rate(0, 1, 0, 11) - 1.0 / 11.0).abs() < f64::EPSILON);
        assert_eq!(change_rate(4, 0, 0, 10), 0.4);
        assert_eq!(change_rate(0, 0, 2, 8), 0.25);
        assert_eq!(change_rate(0, 0, 3, 0), 3.0);
    }

    #[test]
    fn mapping_uses_fingerprint_lcs_for_insertions() {
        let old = (0..10)
            .map(|i| prepared_page(i, &format!("page:{}", i + 1), &format!("fp{i}")))
            .collect::<Vec<_>>();
        let mut new = vec![prepared_page(0, "page:1", "inserted")];
        new.extend(
            (0..10).map(|i| prepared_page(i + 1, &format!("page:{}", i + 2), &format!("fp{i}"))),
        );
        let mapping = map_units(&old, &new);
        assert_eq!(mapping.unchanged.len(), 10);
        assert_eq!(mapping.added_unit_keys, vec!["page:1"]);
        assert!(mapping.changed_unit_keys.is_empty());
        assert!(mapping.removed_unit_keys.is_empty());
    }

    #[test]
    fn mapping_does_not_reuse_identical_content_across_preparation_profiles() {
        let old = vec![prepared_page(0, "page:1", "same-content")];
        let mut new = old.clone();
        new[0].preparation_profile_hash = "sha256:changed-renderer-profile".to_owned();

        let mapping = map_units(&old, &new);
        assert!(mapping.unchanged.is_empty());
        assert_eq!(mapping.changed_unit_keys, vec!["page:1"]);
        assert!(mapping.added_unit_keys.is_empty());
        assert!(mapping.removed_unit_keys.is_empty());
    }

    #[test]
    fn mapping_reuses_identical_content_with_same_preparation_profile() {
        let old = vec![prepared_page(0, "page:1", "same-content")];
        let new = old.clone();

        let mapping = map_units(&old, &new);
        assert_eq!(mapping.unchanged.len(), 1);
        assert!(mapping.changed_unit_keys.is_empty());
        assert!(mapping.added_unit_keys.is_empty());
        assert!(mapping.removed_unit_keys.is_empty());
    }

    #[test]
    fn r23_cand_007_lcs_budget_uses_checked_cell_count() {
        assert!(unit_mapping_within_budget(999, 1_999));
        assert!(!unit_mapping_within_budget(1_000, 1_999));
        assert!(!unit_mapping_within_budget(usize::MAX, 1));
        assert!(!unit_mapping_within_budget(1, usize::MAX));
    }

    #[test]
    fn r23_cand_007_over_budget_mapping_falls_back_to_full_change() {
        let count = 1_414_u64;
        let old = (0..count)
            .map(|i| prepared_page(i, &format!("page:{}", i + 1), &format!("old-{i}")))
            .collect::<Vec<_>>();
        let new = (0..count)
            .map(|i| prepared_page(i, &format!("page:{}", i + 1), &format!("new-{i}")))
            .collect::<Vec<_>>();
        assert!(!unit_mapping_within_budget(old.len(), new.len()));

        let mapping = map_units(&old, &new);
        assert!(mapping.unchanged.is_empty());
        assert_eq!(mapping.changed_unit_keys.len(), count as usize);
        assert!(mapping.added_unit_keys.is_empty());
        assert!(mapping.removed_unit_keys.is_empty());
        assert_eq!(mapping.change_rate, 1.0);
    }

    #[test]
    fn r23_cand_031_prepare_from_verified_bytes_binds_raw_hash() {
        let bytes = b"# trusted\n";
        let raw_hash = hash_bytes(bytes);
        let prepared = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: "text/markdown",
            input_path: "trusted.md",
            tool_profile_hash: "sha256:test",
            bytes,
        })
        .unwrap();
        assert_eq!(prepared.prepared_units.len(), 1);
        assert_eq!(prepared.prepared_units[0].prepared_hash, raw_hash);
        assert_eq!(
            prepared.prepared_units[0].preparation_profile_hash,
            "sha256:test"
        );

        let error = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &hash_bytes(b"different"),
            media_type: "text/markdown",
            input_path: "trusted.md",
            tool_profile_hash: "sha256:test",
            bytes,
        })
        .unwrap_err();
        assert!(error.to_string().contains("KIO-E-PREPARE-IDENTITY-001"));
    }

    #[test]
    fn r23_cand_006_prepare_rejects_pdf_page_count_over_limit() {
        let mut pdf = b"%PDF-1.4\nBT\n".to_vec();
        for id in 1..=kio_adapter::deterministic::MAX_DETERMINISTIC_PDF_PAGES + 1 {
            pdf.extend_from_slice(format!("{id} 0 obj << /Type /Page >> endobj\n").as_bytes());
        }
        let raw_hash = hash_bytes(&pdf);
        let error = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: "application/pdf",
            input_path: "oversized.pdf",
            tool_profile_hash: "sha256:test",
            bytes: &pdf,
        })
        .unwrap_err();
        assert!(error.to_string().contains("KIO-E-PREPARE-PDF-LIMIT-001"));
    }

    #[test]
    fn r23_cand_031_path_wrapper_rejects_bytes_different_from_declared_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mutable.md");
        std::fs::write(&path, b"version-b").unwrap();

        let error = prepare_units(PrepareStageRequest {
            raw_hash: hash_bytes(b"version-a"),
            media_type: "text/markdown".to_owned(),
            input_path: path.display().to_string(),
            tool_profile_hash: "sha256:test".to_owned(),
        })
        .unwrap_err();
        assert!(error.to_string().contains("KIO-E-PREPARE-IDENTITY-001"));
    }

    #[test]
    fn r23_cand_032_stream_hash_matches_in_memory_hash() {
        let bytes = vec![0xa5; 256 * 1024 + 17];
        let streamed = hash_reader(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(streamed, hash_bytes(&bytes));
    }

    // ---- Office (DOCX/PPTX) prepare via converted-PDF intermediate ----------
    #[cfg(debug_assertions)]
    const DOCX_MEDIA_TYPE: &str =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    #[cfg(debug_assertions)]
    const PPTX_MEDIA_TYPE: &str =
        "application/vnd.openxmlformats-officedocument.presentationml.presentation";

    /// Mirrors the hand-crafted uncompressed pseudo-PDF style the existing
    /// PDF-path tests above already use (e.g.
    /// `r23_cand_006_prepare_rejects_pdf_page_count_over_limit`,
    /// `kio-adapter`'s `verified_pdf_bytes_are_reused_for_all_hints`) — a
    /// structural `/Type /Page` marker per page plus a literal `(text) Tj`
    /// content stream the deterministic extractor can read directly.
    #[cfg(debug_assertions)]
    fn multi_page_fixture_pdf(pages: &[&str]) -> Vec<u8> {
        let mut pdf = format!(
            "%PDF-1.4\n1 0 obj << /Type /Pages /Count {} >> endobj\n",
            pages.len()
        )
        .into_bytes();
        for (index, text) in pages.iter().enumerate() {
            pdf.extend_from_slice(
                format!(
                    "{} 0 obj << /Type /Page >> stream\nBT ({text}) Tj ET\nendstream\nendobj\n",
                    index + 2
                )
                .as_bytes(),
            );
        }
        pdf
    }

    #[cfg(debug_assertions)]
    #[test]
    fn office_docx_seam_prepares_page_units_from_converted_pdf() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("converted.pdf");
        let fixture_pdf =
            multi_page_fixture_pdf(&["First converted page", "Second converted page"]);
        std::fs::write(&fixture_path, &fixture_pdf).unwrap();
        let _seam = kio_core::test_control::TestEnvGuard::set(
            "KIO_TEST_OFFICE_CONVERT",
            fixture_path.display().to_string(),
        );
        let _control = kio_core::test_control::install_scoped(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_docx = b"fake docx bytes (content irrelevant under the seam)";
        let raw_hash = hash_bytes(raw_docx);
        let output = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: DOCX_MEDIA_TYPE,
            input_path: "doc.docx",
            tool_profile_hash: "sha256:test",
            bytes: raw_docx,
        })
        .unwrap();

        assert_eq!(output.prepared_units.len(), 2);
        assert_eq!(output.prepared_units[0].unit_key, "page:1");
        assert_eq!(output.prepared_units[0].unit_type, UnitType::Page);
        assert_eq!(output.prepared_units[0].page_number, Some(1));
        // mime stays the ORIGINAL office media type, never application/pdf.
        assert_eq!(
            output.prepared_units[0].mime.as_deref(),
            Some(DOCX_MEDIA_TYPE)
        );
        assert_eq!(output.prepared_units[1].unit_key, "page:2");
        assert_eq!(output.prepared_units[1].page_number, Some(2));

        // prepared_hash derives from the CONVERTED page bytes, not the raw
        // (fake) docx bytes — renderer drift is meant to change this hash.
        let expected_page1_hash = hash_bytes("First converted page".as_bytes());
        assert_eq!(output.prepared_units[0].prepared_hash, expected_page1_hash);
        assert_ne!(output.prepared_units[0].prepared_hash, raw_hash);

        // prepared_object_hashes mirrors the existing application/pdf route
        // exactly: just the per-unit hashes, no separate whole-file entry
        // (the existing route has no analogous whole-file entry either).
        assert_eq!(
            output.prepared_object_hashes,
            output
                .prepared_units
                .iter()
                .map(|unit| unit.prepared_hash.clone())
                .collect::<Vec<_>>()
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn office_pptx_seam_prepares_slide_units_from_converted_pdf() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("converted.pdf");
        let fixture_pdf = multi_page_fixture_pdf(&["First slide", "Second slide", "Third slide"]);
        std::fs::write(&fixture_path, &fixture_pdf).unwrap();
        let _seam = kio_core::test_control::TestEnvGuard::set(
            "KIO_TEST_OFFICE_CONVERT",
            fixture_path.display().to_string(),
        );
        let _control = kio_core::test_control::install_scoped(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_pptx = b"fake pptx bytes (content irrelevant under the seam)";
        let raw_hash = hash_bytes(raw_pptx);
        let output = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: PPTX_MEDIA_TYPE,
            input_path: "deck.pptx",
            tool_profile_hash: "sha256:test",
            bytes: raw_pptx,
        })
        .unwrap();

        assert_eq!(output.prepared_units.len(), 3);
        assert_eq!(output.prepared_units[0].unit_key, "slide:1");
        assert_eq!(output.prepared_units[0].unit_type, UnitType::Slide);
        // Slide does not carry page_number (unchanged from before this change).
        assert_eq!(output.prepared_units[0].page_number, None);
        assert_eq!(output.prepared_units[1].unit_key, "slide:2");
        assert_eq!(output.prepared_units[2].unit_key, "slide:3");
        assert_eq!(
            output.prepared_units[0].mime.as_deref(),
            Some(PPTX_MEDIA_TYPE)
        );
        let expected_slide1_hash = hash_bytes("First slide".as_bytes());
        assert_eq!(output.prepared_units[0].prepared_hash, expected_slide1_hash);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn office_converter_absent_yields_empty_output_not_an_error() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _clear_seam = kio_core::test_control::TestEnvGuard::remove("KIO_TEST_OFFICE_CONVERT");
        let _clear_explicit = kio_core::test_control::TestEnvGuard::remove(
            kio_adapter::office_convert::OFFICE_CONVERTER_ENV,
        );
        // This dev machine has a real soffice on PATH — scrub PATH so this
        // test exercises "no converter available" regardless of environment
        // (also matches CI without LibreOffice installed).
        let _scrub_path =
            kio_core::test_control::TestEnvGuard::set("PATH", "/nonexistent-kio-test-path");
        let _control = kio_core::test_control::install_scoped(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_docx = b"fake docx bytes, no converter available";
        let raw_hash = hash_bytes(raw_docx);
        let output = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: DOCX_MEDIA_TYPE,
            input_path: "doc.docx",
            tool_profile_hash: "sha256:test",
            bytes: raw_docx,
        })
        .unwrap();

        // Same empty shape as the R20-4 scanned-PDF / any other silently-skipped
        // route — prepare stays crash-free (07 §5.1: "doomed task を作らない").
        assert!(output.prepared_units.is_empty());
        assert!(output.prepared_object_hashes.is_empty());
        assert!(output.image_object_hashes.is_empty());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn office_conversion_failure_surfaces_as_a_prepare_contract_error() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        // The seam "converter" points at a fixture path that does not exist:
        // convert_to_pdf's file read fails with AdapterError::ContractViolation,
        // which prepare.rs must map into its OWN KIO-E-PREPARE-OFFICE-CONVERT-001
        // contract error (07 §5.1: joins contract_violation semantics) rather
        // than panicking or silently succeeding.
        let _seam = kio_core::test_control::TestEnvGuard::set(
            "KIO_TEST_OFFICE_CONVERT",
            "/definitely/not/a/real/kio-fixture-path.pdf",
        );
        let _control = kio_core::test_control::install_scoped(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_docx = b"fake docx bytes";
        let raw_hash = hash_bytes(raw_docx);
        let error = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: DOCX_MEDIA_TYPE,
            input_path: "doc.docx",
            tool_profile_hash: "sha256:test",
            bytes: raw_docx,
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("KIO-E-PREPARE-OFFICE-CONVERT-001")
        );
    }

    #[test]
    fn an_unreadable_xlsx_is_an_error_not_a_silently_empty_prepare() {
        // 07 §5.1 (2026-07-25): XLSX now extracts directly instead of being
        // excluded. A workbook we cannot open must say so — returning empty
        // output would index the file as "present, no content", which is
        // exactly how the deferred XLSX used to disappear (backlog I7).
        let raw_xlsx = b"fake xlsx bytes";
        let raw_hash = hash_bytes(raw_xlsx);
        let error = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            input_path: "sheet.xlsx",
            tool_profile_hash: "sha256:test",
            bytes: raw_xlsx,
        })
        .unwrap_err();
        assert!(error.to_string().contains("KIO-E-PREPARE-XLSX-EXTRACT-001"));
    }

    #[test]
    fn qb27_escapes_a_sheet_name_before_numbering_a_duplicate() {
        // 04 §2 L125-128. The order matters: a real sheet named `A#2` must not
        // collide with the second sheet named `A`.
        assert_eq!(
            sheet_unit_keys(&["A#2".to_owned(), "A".to_owned(), "A".to_owned()]),
            vec!["sheet:A##2", "sheet:A", "sheet:A#2"]
        );
        assert_eq!(
            canonical_unit_key(UnitType::Sheet, "Sheet#1"),
            "sheet:Sheet##1"
        );
        // Three sheets sharing a name number in order of appearance.
        assert_eq!(
            sheet_unit_keys(&["s".to_owned(), "s".to_owned(), "s".to_owned()]),
            vec!["sheet:s", "sheet:s#2", "sheet:s#3"]
        );
    }

    #[test]
    fn a_legacy_binary_office_format_still_prepares_nothing() {
        // `.ppt`/`.doc` (the pre-OOXML binaries) are neither `is_office_media`
        // nor XLSX, so they keep hitting the first gate. Pinned so widening the
        // gate for XLSX did not widen it for these.
        let raw = b"fake ppt bytes";
        let raw_hash = hash_bytes(raw);
        let output = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &raw_hash,
            media_type: "application/vnd.ms-powerpoint",
            input_path: "deck.ppt",
            tool_profile_hash: "sha256:test",
            bytes: raw,
        })
        .unwrap();
        assert!(output.prepared_units.is_empty());
        assert!(output.prepared_object_hashes.is_empty());
        assert!(output.image_object_hashes.is_empty());
    }
}
