//! Prepare stage contracts.

use kio_core::control::ControlHandle;
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
pub struct RendererIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedUnit {
    /// Renderer actually used to produce these bytes; absent on direct extraction.
    pub renderer: Option<RendererIdentity>,
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
    if crate::unsupported::unsupported_reason_for_path(Path::new(&request.input_path)).is_some() {
        return Ok(empty_prepare_output());
    }
    // R20-6: the local deterministic adapter only parses text (Markdown / plain text) and PDF text layers (docs/07 §7.1). A RECOGNIZED non-text-native binary (image
    // / OOXML office document) is never locally parseable — a raw-bytes "passthrough" unit
    // evidences its bytes (a ZIP's PK header, an image's pixels) as searchable garbage.
    // Route it to online OCR (empty prepared_units → the R20-5 path). `octet-stream`
    // (unrecognized extension) is content-sniffed below: it may be a TEXT file with an
    // unknown extension (`.env`, `.cfg`) — still passthrough'd locally — or a binary blob.
    let media_type = request.media_type.as_str();
    let is_text_native = matches!(media_type, "text/markdown" | "text/plain");
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
    prepare_units_from_bytes_with_control(request, &ControlHandle::default())
}

/// Invocation-local resource allowance shared by staged extraction and preview.
#[derive(Debug, Clone, Copy)]
pub struct ExtractionBudget {
    tier: kio_core::error::ResourceTier,
}

impl ExtractionBudget {
    pub fn new(tier: kio_core::error::ResourceTier) -> Self {
        Self { tier }
    }

    pub fn memory_bytes(self) -> u64 {
        match self.tier {
            kio_core::error::ResourceTier::Standard => 256 * 1024 * 1024,
            kio_core::error::ResourceTier::Large => 1024 * 1024 * 1024,
        }
    }

    /// Check the bytes simultaneously held by the extraction producer.
    pub fn check_retained_bytes(self, observed: u64) -> Result<()> {
        let limit = self.memory_bytes();
        if observed > limit {
            return Err(crate::PipelineError::ResourceLimit(
                kio_core::error::ResourceLimit {
                    resource: kio_core::error::Resource::Memory,
                    stage: "extract".to_owned(),
                    limit: Some(limit),
                    observed: Some(observed),
                    resource_tier: Some(self.tier),
                    page_progress: None,
                },
            ));
        }
        Ok(())
    }
}

pub fn prepare_units_from_bytes_with_control(
    request: PrepareStageBytesRequest<'_>,
    control: &ControlHandle,
) -> Result<PrepareStageOutput> {
    prepare_units_with_budget(
        request,
        control,
        ExtractionBudget::new(kio_core::error::ResourceTier::Standard),
    )
}

/// Run the same local producer as staged extraction without publishing its units.
pub fn prepare_units_with_budget(
    request: PrepareStageBytesRequest<'_>,
    control: &ControlHandle,
    budget: ExtractionBudget,
) -> Result<PrepareStageOutput> {
    visit_prepared_units_with_budget(
        request,
        control,
        budget,
        &mut XlsxExtractionProgress::default(),
        |_, _| Ok(()),
    )
}

/// Workbook-order sheet counts and original worksheet row coordinates.
#[derive(Debug, Serialize, Deserialize)]
pub struct XlsxExtractionProgress {
    pub total_sheets: Option<usize>,
    pub completed_sheets: usize,
    pub first_unprocessed_sheet: usize,
    pub current_sheet: Option<String>,
    pub parsed_through_row: u64,
    pub completed_through_row: u64,
    pub first_unprocessed_row: Option<u64>,
}

impl Default for XlsxExtractionProgress {
    fn default() -> Self {
        Self {
            total_sheets: None,
            completed_sheets: 0,
            first_unprocessed_sheet: 1,
            current_sheet: None,
            parsed_through_row: 0,
            completed_through_row: 0,
            first_unprocessed_row: None,
        }
    }
}

/// Run local extraction with the same producer used by staged persistence.
pub fn visit_prepared_units_with_budget(
    request: PrepareStageBytesRequest<'_>,
    control: &ControlHandle,
    budget: ExtractionBudget,
    progress: &mut XlsxExtractionProgress,
    mut visit: impl FnMut(&PreparedUnit, &[u8]) -> Result<()>,
) -> Result<PrepareStageOutput> {
    let converter = resolve_prepare_converter(request.media_type, control);
    prepare_units_emitting(
        request,
        converter.as_ref(),
        budget,
        &[],
        progress,
        &mut PreparedUnitSink {
            declare: &mut |_| Ok(()),
            write: &mut |unit, bytes, _| visit(unit, bytes),
            read_sheet_usage: &|_| unreachable!("preview has no staged worksheet"),
            record_stages: None,
        },
    )
}

fn resolve_prepare_converter(
    media_type: &str,
    control: &ControlHandle,
) -> Option<kio_adapter::office_convert::OfficeConverter> {
    if kio_adapter::office_convert::is_office_media(media_type) {
        kio_adapter::office_convert::resolve_office_converter_with_control(control).ok()
    } else {
        None
    }
}

fn prepare_units_emitting(
    request: PrepareStageBytesRequest<'_>,
    converter: Option<&kio_adapter::office_convert::OfficeConverter>,
    budget: ExtractionBudget,
    completed: &[PreparedUnit],
    progress: &mut XlsxExtractionProgress,
    emit: &mut PreparedUnitSink<'_>,
) -> Result<PrepareStageOutput> {
    if crate::unsupported::unsupported_reason_for_path(Path::new(request.input_path)).is_some() {
        return Ok(empty_prepare_output());
    }
    let media_type = request.media_type;
    let is_text_native = matches!(media_type, "text/markdown" | "text/plain");
    let is_native_image = matches!(media_type, "image/png" | "image/jpeg" | "image/webp");
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
        && !is_native_image
        && !is_pdf
        && !is_office
        && !is_xlsx
        && media_type != "application/octet-stream"
    {
        return Ok(empty_prepare_output());
    }
    // Single-unit sources know their complete layout before generating a body.
    if !is_pdf
        && !is_office
        && !is_xlsx
        && let [unit] = completed
    {
        return Ok(PrepareStageOutput {
            prepared_object_hashes: vec![unit.prepared_hash.clone()],
            prepared_units: vec![unit.clone()],
            image_object_hashes: Vec::new(),
        });
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
    if unsupported_reason_for_content(media_type, bytes).is_some() {
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
        const CODE: &str = "KIO-E-PREPARE-XLSX-EXTRACT-001";
        *progress = XlsxExtractionProgress::default();
        let mut workbook = kio_adapter::xlsx_extract::XlsxWorkbook::open_with_stages(
            bytes,
            budget.tier,
            emit.record_stages,
        )
        .map_err(|err| prepare_adapter_error(CODE, err))?;
        let unit_keys = sheet_unit_keys(workbook.sheet_names());
        let total = unit_keys.len();
        progress.total_sheets = Some(total);
        emit.declare(total)?;
        let preparation_profile_hash = xlsx_preparation_profile_hash(&preparation_profile_hash);
        let mut prepared_units = Vec::with_capacity(total);
        for (index, unit_key) in unit_keys.into_iter().enumerate() {
            progress.current_sheet = Some(unit_key.clone());
            progress.parsed_through_row = 0;
            progress.completed_through_row = 0;
            progress.first_unprocessed_row = None;
            if let Some(unit) = completed.get(index) {
                if unit.unit_key != unit_key {
                    return Err(crate::PipelineError::contract(
                        CODE,
                        "staged worksheet identity does not match the workbook",
                    ));
                }
                let usage = (emit.read_sheet_usage)(unit.order)?;
                workbook
                    .reuse_sheet(usage)
                    .map_err(|err| prepare_adapter_error(CODE, err))?;
                prepared_units.push(unit.clone());
            } else {
                let mut rows = kio_adapter::xlsx_extract::SheetProgress::default();
                let result = workbook.read_sheet(index, &mut rows);
                progress.parsed_through_row = rows.parsed_through_row;
                progress.completed_through_row = rows.completed_through_row;
                progress.first_unprocessed_row = rows.first_unprocessed_row;
                let (markdown, usage) = result.map_err(|err| prepare_adapter_error(CODE, err))?;
                let markdown = markdown.as_bytes();
                let unit = PreparedUnit {
                    renderer: None,
                    order: index as u64,
                    unit_key,
                    unit_type: UnitType::Sheet,
                    prepared_hash: hash_bytes(markdown),
                    preparation_profile_hash: preparation_profile_hash.clone(),
                    fingerprint: fingerprint_for_bytes(markdown, markdown),
                    mime: Some(request.media_type.to_owned()),
                    page_number: None,
                };
                progress.first_unprocessed_row = None;
                emit.write_sheet(&unit, markdown, usage)?;
                prepared_units.push(unit);
            }
            progress.completed_sheets += 1;
            progress.first_unprocessed_sheet = index + 2;
        }
        return Ok(PrepareStageOutput {
            prepared_object_hashes: prepared_units
                .iter()
                .map(|unit| unit.prepared_hash.clone())
                .collect(),
            prepared_units,
            image_object_hashes: Vec::new(),
        });
    }
    // 07 §5.1 (2026-07-23 addendum): DOCX/PPTX unit-ize via a converted-PDF
    // intermediate produced by an external renderer (LibreOffice `soffice`).
    // The index caller records unavailable rendering and decides whether
    // empty preparation can fall back to OCR.
    // A runtime conversion failure surfaces as a prepare-stage contract
    // error (07 §5.1: "実行時の変換失敗は contract_violation に合流する" — any
    // KIO-E-PREPARE-* code the task-retry classifier does not specifically
    // recognize falls through to the `ContractViolation` bucket already,
    // `kio_pipeline::task::task_retry_kind`'s catch-all arm).
    let mut renderer = None;
    let office_converted_pdf = if is_office {
        match converter {
            None => return Ok(empty_prepare_output()),
            Some(converter) => {
                renderer = Some(RendererIdentity {
                    name: "LibreOffice".to_owned(),
                    version: converter.version().to_owned(),
                });
                preparation_profile_hash = office_preparation_profile_hash(
                    request.tool_profile_hash,
                    &converter.profile_identity(),
                );
                Some(converter.convert_to_pdf(bytes, media_type).map_err(|err| {
                    prepare_adapter_error("KIO-E-PREPARE-OFFICE-CONVERT-001", err)
                })?)
            }
        }
    } else {
        None
    };
    if is_pdf || is_office {
        let pdf = office_converted_pdf.as_deref().unwrap_or(bytes);
        let pdf_error = |err| prepare_adapter_error("KIO-E-PREPARE-PDF-EXTRACT-001", err);
        let mut pages =
            kio_adapter::pdf_decode::PdfPages::open(pdf, budget.tier).map_err(pdf_error)?;
        let count = pages.len();
        let annotate = |error, extracted_through| {
            page_extraction_error(error, count, extracted_through, budget.tier)
        };
        let mut prepared_units = completed.to_vec();
        let mut declared = !completed.is_empty();
        if declared {
            emit.declare(count)
                .map_err(|err| annotate(err, completed.len()))?;
        }
        for index in completed.len()..count {
            let page = pages
                .read_page(index)
                .map_err(pdf_error)
                .map_err(|err| annotate(err, index))?;
            let real = is_probably_real_text(&page);
            if !declared {
                if !real {
                    continue;
                }
                emit.declare(count)
                    .map_err(|err| annotate(err, index + 1))?;
                declared = true;
            }
            // Once there is a text layer, preserve blank pages and slides as
            // units. Scanned documents with no text layer leave an empty stage.
            for order in prepared_units.len()..=index {
                let body = if order == index && real {
                    page.as_bytes()
                } else {
                    b""
                };
                let page_number = order as u64 + 1;
                let unit = PreparedUnit {
                    renderer: renderer.clone(),
                    order: order as u64,
                    unit_key: canonical_unit_key(unit_type, &page_number.to_string()),
                    unit_type,
                    prepared_hash: hash_bytes(body),
                    preparation_profile_hash: preparation_profile_hash.clone(),
                    fingerprint: fingerprint_for_bytes(body, body),
                    mime: Some(media_type.to_owned()),
                    page_number: (unit_type == UnitType::Page).then_some(page_number),
                };
                emit.write(&unit, body)
                    .map_err(|err| annotate(err, index + 1))?;
                prepared_units.push(unit);
            }
        }
        return Ok(PrepareStageOutput {
            prepared_object_hashes: prepared_units
                .iter()
                .map(|unit| unit.prepared_hash.clone())
                .collect(),
            prepared_units,
            image_object_hashes: Vec::new(),
        });
    }
    budget.check_retained_bytes(bytes.len() as u64)?;
    emit.declare(1)?;
    let unit = PreparedUnit {
        renderer: None,
        order: 0,
        unit_key: canonical_unit_key(
            unit_type,
            if unit_type == UnitType::Image {
                "0"
            } else {
                "1"
            },
        ),
        unit_type,
        fingerprint: fingerprint_for_bytes(bytes, prepared_hash.as_bytes()),
        prepared_hash: prepared_hash.clone(),
        preparation_profile_hash,
        mime: Some(media_type.to_owned()),
        page_number: None,
    };
    emit.write(&unit, bytes)?;
    Ok(PrepareStageOutput {
        prepared_object_hashes: vec![prepared_hash],
        prepared_units: vec![unit],
        image_object_hashes: Vec::new(),
    })
}

fn page_extraction_error(
    error: crate::PipelineError,
    total_pages: usize,
    extracted_through: usize,
    tier: kio_core::error::ResourceTier,
) -> crate::PipelineError {
    use kio_core::error::{PageExtractionProgress, PageExtractionRetry, Resource, ResourceTier};

    let crate::PipelineError::ResourceLimit(mut limit) = error else {
        return error;
    };
    let retry = match (limit.resource, tier) {
        (Resource::Storage, ResourceTier::Standard) => PageExtractionRetry::FreeStorage,
        (Resource::Storage, ResourceTier::Large) => PageExtractionRetry::FreeStorageThenIndexLarge,
        (_, ResourceTier::Standard) => PageExtractionRetry::TryLarge,
        (Resource::UnitText, ResourceTier::Large) => PageExtractionRetry::ReducePageText,
        (_, ResourceTier::Large) => PageExtractionRetry::ReducePdfComplexity,
    };
    limit.page_progress = Some(Box::new(PageExtractionProgress {
        total_pages: total_pages as u64,
        extracted_through: extracted_through as u64,
        staged_through: None,
        next_page: (extracted_through < total_pages).then(|| extracted_through as u64 + 1),
        retry,
    }));
    crate::PipelineError::ResourceLimit(limit)
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
pub fn sheet_unit_keys<I>(names: I) -> Vec<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut seen = std::collections::BTreeMap::new();
    names
        .into_iter()
        .map(|name| {
            let base = canonical_unit_key(UnitType::Sheet, name.as_ref());
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

fn xlsx_preparation_profile_hash(profile: &str) -> String {
    hash_bytes(format!("{profile}\0xlsx-streaming-resources").as_bytes())
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

pub fn unsupported_reason_for_content(media_type: &str, bytes: &[u8]) -> Option<&'static str> {
    (media_type == "application/octet-stream" && !bytes_are_text(bytes))
        .then_some(crate::unsupported::UNSUPPORTED_REASON_UNRECOGNIZED_BINARY)
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

fn prepare_adapter_error(
    code: &'static str,
    error: kio_adapter::AdapterError,
) -> crate::PipelineError {
    match error {
        kio_adapter::AdapterError::ResourceLimit(limit) => {
            crate::PipelineError::ResourceLimit(limit)
        }
        kio_adapter::AdapterError::RecordStage(error) => crate::PipelineError::RecordStage(error),
        other => crate::PipelineError::contract(code, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_preserves_xlsx_spill_io_failure() {
        let code = "KIO-E-PREPARE-XLSX-EXTRACT-001";
        let error = kio_core::KioError::io("worksheet spill write failed", "record-stages/records");
        let original = error.to_error_json();
        let actual = prepare_adapter_error(code, kio_adapter::AdapterError::RecordStage(error));
        assert!(
            matches!(actual, crate::PipelineError::RecordStage(error) if error.to_error_json() == original)
        );
    }

    #[test]
    fn source_code_extensions_never_prepare_as_sniffed_text() {
        let bytes = b"sourceonlyneedle readable text\n";
        let raw_hash = hash_bytes(bytes);
        for ext in ["rs", "py", "js", "ts", "go", "java", "c", "h", "cpp"] {
            for ext in [ext.to_owned(), ext.to_ascii_uppercase()] {
                let input_path = format!("main.{ext}");
                for media_type in ["application/octet-stream", "text/plain"] {
                    let output = prepare_units_from_bytes(PrepareStageBytesRequest {
                        raw_hash: &raw_hash,
                        media_type,
                        input_path: &input_path,
                        tool_profile_hash: "sha256:test",
                        bytes,
                    })
                    .unwrap();
                    assert!(
                        output.prepared_units.is_empty(),
                        "{input_path}: {media_type}"
                    );
                    let output = prepare_units(PrepareStageRequest {
                        raw_hash: raw_hash.clone(),
                        media_type: media_type.to_owned(),
                        input_path: input_path.clone(),
                        tool_profile_hash: "sha256:test".to_owned(),
                    })
                    .unwrap();
                    assert!(output.prepared_units.is_empty());
                }
            }
        }
    }

    fn prepared_page(order: u64, key: &str, fp: &str) -> PreparedUnit {
        PreparedUnit {
            renderer: None,
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
    fn pdf_prepare_visits_text_beyond_the_old_page_and_object_limits() {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        for id in 1..=257 {
            pdf.extend_from_slice(
                format!("{id} 0 obj << /Type /Page /Contents 258 0 R >> endobj\n").as_bytes(),
            );
        }
        pdf.extend_from_slice(
            b"258 0 obj << /Length 17 >> stream\nBT (page text) Tj\nendstream\nendobj\n",
        );
        for id in 259..=8193 {
            pdf.extend_from_slice(format!("{id} 0 obj null endobj\n").as_bytes());
        }
        let mut visited = 0;
        let output = visit_prepared_units_with_budget(
            PrepareStageBytesRequest {
                raw_hash: &hash_bytes(&pdf),
                media_type: "application/pdf",
                input_path: "many-pages.pdf",
                tool_profile_hash: "sha256:test",
                bytes: &pdf,
            },
            &ControlHandle::default(),
            ExtractionBudget::new(kio_core::error::ResourceTier::Standard),
            &mut XlsxExtractionProgress::default(),
            |unit, bytes| {
                visited += 1;
                assert_eq!(unit.page_number, Some(visited));
                assert_eq!(bytes, b"page text");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(visited, 257);
        assert_eq!(output.prepared_units.len(), 257);
    }

    #[test]
    fn p1_review2_large_unit_text_hold_recommends_reducing_the_page() {
        use kio_core::error::{Resource, ResourceLimit, ResourceTier};
        let error = crate::PipelineError::ResourceLimit(ResourceLimit {
            resource: Resource::UnitText,
            stage: "extract".to_owned(),
            limit: Some(64 * 1024 * 1024),
            observed: Some(64 * 1024 * 1024 + 1),
            resource_tier: Some(ResourceTier::Large),
            page_progress: None,
        });
        let crate::PipelineError::ResourceLimit(limit) =
            page_extraction_error(error, 4, 2, ResourceTier::Large)
        else {
            panic!("missing extraction hold");
        };
        assert_eq!(
            limit.page_progress.unwrap().retry,
            kio_core::error::PageExtractionRetry::ReducePageText
        );
    }

    #[test]
    fn pdf_prepare_limits_unit_text_and_emits_prefix_before_failure() {
        use kio_core::error::{Resource, ResourceTier};
        const STANDARD_TEXT: usize = 16 * 1024 * 1024;
        for (length, tier, accepted) in [
            (STANDARD_TEXT, ResourceTier::Standard, true),
            (STANDARD_TEXT + 1, ResourceTier::Standard, false),
            (STANDARD_TEXT + 1, ResourceTier::Large, true),
        ] {
            let mut pdf = b"%PDF-1.4\n1 0 obj << /Type /Page >> stream\nBT (first) Tj\nendstream\nendobj\n2 0 obj << /Type /Page /Contents 3 0 R >> endobj\n".to_vec();
            pdf.extend_from_slice(
                format!("3 0 obj << /Length {} >> stream\nBT (", length + 8).as_bytes(),
            );
            pdf.resize(pdf.len() + length, b'x');
            pdf.extend_from_slice(b") Tj\nendstream\nendobj\n");
            let mut lengths = Vec::new();
            let result = visit_prepared_units_with_budget(
                PrepareStageBytesRequest {
                    raw_hash: &hash_bytes(&pdf),
                    media_type: "application/pdf",
                    input_path: "text-budget.pdf",
                    tool_profile_hash: "sha256:test",
                    bytes: &pdf,
                },
                &ControlHandle::default(),
                ExtractionBudget::new(tier),
                &mut XlsxExtractionProgress::default(),
                |_, bytes| {
                    lengths.push(bytes.len());
                    Ok(())
                },
            );
            if accepted {
                assert_eq!(result.unwrap().prepared_units.len(), 2);
                assert_eq!(lengths, [5, length]);
            } else {
                let Err(crate::PipelineError::ResourceLimit(limit)) = result else {
                    panic!("expected the unit text resource limit, got {result:?}");
                };
                assert_eq!(limit.resource, Resource::UnitText);
                assert_eq!(limit.resource_tier, Some(tier));
                assert_eq!(limit.limit, Some(STANDARD_TEXT as u64));
                assert_eq!(limit.observed, Some(length as u64));
                assert_eq!(lengths, [5]);
            }
        }
    }

    #[test]
    fn pdf_prepare_keeps_unsupported_filter_empty() {
        let pdf = b"%PDF-1.4\n1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n2 0 obj << /Length 21 /Filter /DCTDecode >> stream\nBT (foreign bytes) Tj\nendstream\nendobj\n";
        let output = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &hash_bytes(pdf),
            media_type: "application/pdf",
            input_path: "unsupported-filter.pdf",
            tool_profile_hash: "sha256:test",
            bytes: pdf,
        })
        .unwrap();
        assert!(output.prepared_units.is_empty());
    }

    #[test]
    fn pdf_prepare_reports_a_cyclic_page_tree_as_malformed() {
        let pdf = b"%PDF-1.4\n1 0 obj << /Type /Pages /Kids [2 0 R] >> endobj\n2 0 obj << /Type /Pages /Kids [1 0 R] >> endobj\n";
        let error = prepare_units_from_bytes(PrepareStageBytesRequest {
            raw_hash: &hash_bytes(pdf),
            media_type: "application/pdf",
            input_path: "cyclic.pdf",
            tool_profile_hash: "sha256:test",
            bytes: pdf,
        })
        .unwrap_err();
        assert!(error.to_string().contains("cycle"), "{error}");
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
    fn pdf_prepare_streams_large_converted_office_documents() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("converted.pdf");
        std::fs::write(&path, multi_page_fixture_pdf(&["Converted page"; 257])).unwrap();
        let _seam = kio_core::test_control::TestEnvGuard::set(
            "KIO_TEST_OFFICE_CONVERT",
            path.display().to_string(),
        );
        let control = ControlHandle::new(kio_core::test_control::DebugTestControl::from_env());
        for (media_type, input_path, unit_type, final_key) in [
            (DOCX_MEDIA_TYPE, "large.docx", UnitType::Page, "page:257"),
            (PPTX_MEDIA_TYPE, "large.pptx", UnitType::Slide, "slide:257"),
        ] {
            let bytes = b"input handled by the renderer seam";
            let mut count = 0;
            let output = visit_prepared_units_with_budget(
                PrepareStageBytesRequest {
                    raw_hash: &hash_bytes(bytes),
                    media_type,
                    input_path,
                    tool_profile_hash: "sha256:test",
                    bytes,
                },
                &control,
                ExtractionBudget::new(kio_core::error::ResourceTier::Standard),
                &mut XlsxExtractionProgress::default(),
                |unit, bytes| {
                    assert_eq!(unit.order, count);
                    assert_eq!(unit.unit_type, unit_type);
                    assert_eq!(bytes, b"Converted page");
                    count += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(count, 257);
            assert_eq!(output.prepared_units.last().unwrap().unit_key, final_key);
        }
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
        let control = kio_core::control::ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_docx = b"fake docx bytes (content irrelevant under the seam)";
        let raw_hash = hash_bytes(raw_docx);
        let output = prepare_units_from_bytes_with_control(
            PrepareStageBytesRequest {
                raw_hash: &raw_hash,
                media_type: DOCX_MEDIA_TYPE,
                input_path: "doc.docx",
                tool_profile_hash: "sha256:test",
                bytes: raw_docx,
            },
            &control,
        )
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
        let control = kio_core::control::ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_pptx = b"fake pptx bytes (content irrelevant under the seam)";
        let raw_hash = hash_bytes(raw_pptx);
        let output = prepare_units_from_bytes_with_control(
            PrepareStageBytesRequest {
                raw_hash: &raw_hash,
                media_type: PPTX_MEDIA_TYPE,
                input_path: "deck.pptx",
                tool_profile_hash: "sha256:test",
                bytes: raw_pptx,
            },
            &control,
        )
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
        let control = kio_core::control::ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_docx = b"fake docx bytes, no converter available";
        let raw_hash = hash_bytes(raw_docx);
        let output = prepare_units_from_bytes_with_control(
            PrepareStageBytesRequest {
                raw_hash: &raw_hash,
                media_type: DOCX_MEDIA_TYPE,
                input_path: "doc.docx",
                tool_profile_hash: "sha256:test",
                bytes: raw_docx,
            },
            &control,
        )
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
        let control = kio_core::control::ControlHandle::new(
            kio_core::test_control::DebugTestControl::from_env(),
        );

        let raw_docx = b"fake docx bytes";
        let raw_hash = hash_bytes(raw_docx);
        let error = prepare_units_from_bytes_with_control(
            PrepareStageBytesRequest {
                raw_hash: &raw_hash,
                media_type: DOCX_MEDIA_TYPE,
                input_path: "doc.docx",
                tool_profile_hash: "sha256:test",
                bytes: raw_docx,
            },
            &control,
        )
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

mod staged;
pub use staged::{
    PreparedUnitSink, StagedPreparation, discard_prepared_stages, prepare_staged,
    prepare_staged_with, prepare_staged_with_converter, publish_prepared_source,
};
