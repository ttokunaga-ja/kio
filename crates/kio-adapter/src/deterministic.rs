//! Built-in deterministic adapter skeleton.

use crate::pdf_decode::PdfPages;
use crate::traits::{MarkdownizeAdapter, PrepareAdapter};
use crate::types::{
    AdapterKind, AdapterProfile, ExecutionMode, MarkdownUnit, MarkdownizeMode, MarkdownizeRequest,
    MarkdownizeResponse, PrepareRequest, PrepareResponse, PreparedUnitHint, PreparedUnitMetadata,
    UnitFingerprint, UnitKind,
};
use crate::{AdapterError, Result};
use kio_core::error::ResourceTier;
use serde_json::json;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Default)]
pub struct DeterministicAdapter;

impl DeterministicAdapter {
    fn profile_for(adapter_kind: AdapterKind) -> AdapterProfile {
        let (profile_input, capability_flags) = match adapter_kind {
            AdapterKind::Prepare => (deterministic_prepare_profile_value(), Vec::new()),
            AdapterKind::Markdownize => (
                deterministic_markdown_profile_value(),
                vec!["baseline".to_owned(), "text_passthrough".to_owned()],
            ),
            _ => (
                json!({
                    "adapter_kind": "markdownize",
                    "adapter_role": "text",
                    "model_or_tool_family": "kio-deterministic-text",
                    "model_version_pin": "1.0.0",
                    "output_schema": "kio-markdown-v1",
                    "runtime_kind": "local",
                    "spec_version": 1
                }),
                Vec::new(),
            ),
        };
        let tool_profile_hash = crate::identity::tool_profile_hash(&profile_input)
            .expect("built-in deterministic profile is valid");
        AdapterProfile {
            adapter_kind,
            adapter_id: "deterministic_builtin".to_owned(),
            execution_mode: ExecutionMode::DeterministicLibrary,
            tool_profile_hash,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capability_flags,
            allow_network: false,
            // The built-in deterministic adapter never bills (local, no
            // network) — QA18's `billable_kinds`/`reject_billing` are only
            // REQUIRED for a billable adapter (07 §5.5 condition 6). Per the
            // QA18 ruling, a non-billable adapter still states its billing
            // posture explicitly (`Nonbillable`) rather than leaving
            // `reject_billing` at `None` — a consumer must fail-closed
            // interpret an absent declaration as "billable" (07 §4: "欠落・
            // 未知値は fail-closed = billable として扱う"), so declaring is
            // the only way to say "this adapter does not bill".
            billable_kinds: Vec::new(),
            reject_billing: Some(crate::types::BillingDeclaration::Nonbillable),
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): this
            // adapter is local/offline (never a sync provider call), so
            // there is no provider idempotency posture to declare beyond the
            // default.
            provider_idempotency: crate::types::ProviderIdempotency::NotProvided,
        }
    }
}

pub fn deterministic_markdown_profile_value() -> serde_json::Value {
    // 1.1.0 (2026-07-23): FlateDecode + ToUnicode CMap decoding changed what
    // this adapter extracts from compressed text-layer PDFs.
    // 1.2.0 (2026-07-23, same day): non-markdown text (text/plain + sniffed
    // octet-stream TEXT) is now fence-wrapped — raw passthrough violated the
    // v1 "raw HTML forbidden" acceptance for XML/HTML corpus files.
    // 1.3.0 (2026-07-23, same day): multi-page markdown follows the graph
    // decoder's page authority (ObjStm-aware) — see the prepare 1.2.0 note.
    // 1.4.0: use page-local fonts and the resource-tiered PDF decoder.
    // 1.5.0: follow active xref and Kids order.
    json!({
        "adapter_kind": "markdownize",
        "adapter_role": "text",
        "model_or_tool_family": "kio-deterministic-text",
        "model_version_pin": "1.5.0",
        "output_schema": "kio-markdown-v1",
        "runtime_kind": "local",
        "spec_version": 1
    })
}

pub fn deterministic_prepare_profile_value() -> serde_json::Value {
    // 1.1.0 (2026-07-23): FlateDecode/ToUnicode bump (extracted page text).
    // 1.2.0 (2026-07-23, same day): the graph decoder's page count became
    // the page authority — the raw structural scan is ObjStm-blind and
    // truncated multi-page TeX PDFs to one prepared unit. Unit cardinality
    // changes => prepare identity moves (07 §9).
    // 1.3.0: use resource-tiered PDF extraction and fail empty on unsupported filters.
    // 1.4.0: follow active xref and Kids order.
    json!({
        "adapter_kind": "prepare",
        "adapter_role": "text",
        "model_or_tool_family": "kio-deterministic-prepare",
        "model_version_pin": "1.4.0",
        "runtime_kind": "local",
        "spec_version": 1
    })
}

impl PrepareAdapter for DeterministicAdapter {
    fn profile(&self) -> AdapterProfile {
        Self::profile_for(AdapterKind::Prepare)
    }

    fn prepare(&self, request: PrepareRequest) -> Result<PrepareResponse> {
        let unit_kind = unit_kind_for_media_type(&request.media_type);
        let unit_key = match unit_kind {
            UnitKind::Page => "page:1",
            UnitKind::Image => "image:0",
            UnitKind::Sheet => "sheet:Sheet1",
            UnitKind::Slide => "slide:1",
            UnitKind::File | UnitKind::HeadingSection | UnitKind::Symbol => "doc:1",
        }
        .to_owned();
        let fingerprint = UnitFingerprint {
            perceptual_hash: request.raw_hash.clone(),
            text_hash: request.raw_hash.clone(),
            visual_hash: request.raw_hash.clone(),
        };
        Ok(PrepareResponse {
            prepared_object_hashes: vec![request.raw_hash.clone()],
            prepared_unit_hashes: vec![request.raw_hash.clone()],
            image_object_hashes: Vec::new(),
            metadata: vec![PreparedUnitMetadata {
                unit_key,
                unit_kind,
                page_number: matches!(unit_kind, UnitKind::Page).then_some(1),
                mime: Some(request.media_type),
                fingerprint,
            }],
        })
    }
}

impl MarkdownizeAdapter for DeterministicAdapter {
    fn profile(&self) -> AdapterProfile {
        Self::profile_for(AdapterKind::Markdownize)
    }

    fn markdownize(&self, request: MarkdownizeRequest) -> Result<MarkdownizeResponse> {
        match request.raw.path.as_deref() {
            Some(path) => {
                let bytes = std::fs::read(path).map_err(|err| AdapterError::Io {
                    path: path.to_owned(),
                    message: err.to_string(),
                })?;
                markdownize_from_bytes(request, &bytes)
            }
            None => markdownize_with_source(request, None),
        }
    }
}

/// Markdownize caller-owned bytes without reopening a pathname. This is the
/// deterministic counterpart to the online verified-bytes API.
pub fn markdownize_from_bytes(
    request: MarkdownizeRequest,
    verified_raw_bytes: &[u8],
) -> Result<MarkdownizeResponse> {
    let source = source_document_from_verified_bytes(&request, verified_raw_bytes)?;
    markdownize_with_source(request, Some(source))
}

/// Markdownize a converted PDF using the same page reader as native PDF.
pub fn markdownize_converted_office(
    request: MarkdownizeRequest,
    converted_pdf: &[u8],
    unit_kind: UnitKind,
) -> Result<MarkdownizeResponse> {
    let source = SourceDocument::Pdf {
        pages: Box::new(PdfPages::open(converted_pdf, ResourceTier::Standard)?),
        office_kind: Some(unit_kind),
    };
    markdownize_with_source(request, Some(source))
}

enum SourceDocument<'a> {
    Text(String),
    Pdf {
        pages: Box<PdfPages<'a>>,
        office_kind: Option<UnitKind>,
    },
    /// XLSX worksheets, already rendered to Markdown by
    /// [`crate::xlsx_extract`], in workbook order. A `sheet:` unit resolves by
    /// ORDER rather than by name: the unit key carries QB27's escaping and
    /// duplicate-occurrence suffix, so matching the raw sheet name against it
    /// would need that transform reapplied here — and the prepare stage
    /// already emitted the units in this exact order.
    Sheets(Vec<String>),
}

fn source_document_from_verified_bytes<'a>(
    request: &MarkdownizeRequest,
    bytes: &'a [u8],
) -> Result<SourceDocument<'a>> {
    let actual_hash = crate::identity::hash_bytes(bytes);
    if actual_hash != request.raw.raw_hash {
        return Err(AdapterError::ContractViolation(format!(
            "deterministic input identity changed: expected {}, got {actual_hash}",
            request.raw.raw_hash
        )));
    }
    if request.media_type == "application/pdf" {
        return Ok(SourceDocument::Pdf {
            pages: Box::new(PdfPages::open(bytes, ResourceTier::Standard)?),
            office_kind: None,
        });
    }
    if crate::xlsx_extract::is_xlsx_media(&request.media_type) {
        return crate::xlsx_extract::extract_xlsx(bytes).map(|document| {
            SourceDocument::Sheets(
                document
                    .sheets
                    .into_iter()
                    .map(|sheet| sheet.markdown)
                    .collect(),
            )
        });
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    let text = text
        .strip_prefix('\u{feff}')
        .map(str::to_owned)
        .unwrap_or(text);
    Ok(SourceDocument::Text(text))
}

fn markdownize_with_source(
    request: MarkdownizeRequest,
    mut source: Option<SourceDocument<'_>>,
) -> Result<MarkdownizeResponse> {
    let hints = request.prepared_unit_hint.clone().unwrap_or_else(|| {
        if let Some(SourceDocument::Pdf {
            pages,
            office_kind: Some(kind),
        }) = &source
        {
            let prefix = if *kind == UnitKind::Slide {
                "slide"
            } else {
                "page"
            };
            (0..pages.len().max(1))
                .map(|index| PreparedUnitHint {
                    unit_key: format!("{prefix}:{}", index + 1),
                    prepared_hash: String::new(),
                    unit_kind: *kind,
                    order: index as u64,
                })
                .collect()
        } else {
            vec![default_hint(&request.raw.raw_hash)]
        }
    });
    if request.mode == MarkdownizeMode::Incremental {
        let incremental = request.hints.clone();
        let changed = incremental
            .as_ref()
            .map(|hints| hints.changed_unit_keys.as_slice())
            .unwrap_or(&[]);
        let added = incremental
            .as_ref()
            .map(|hints| hints.added_unit_keys.as_slice())
            .unwrap_or(&[]);
        return Ok(MarkdownizeResponse {
            mode_used: MarkdownizeMode::Incremental,
            updated_units: hints
                .iter()
                .filter(|hint| changed.contains(&hint.unit_key))
                .map(|hint| markdown_unit_from_hint(hint, &request, source.as_mut()))
                .collect::<Result<_>>()?,
            unchanged_unit_keys: Vec::new(),
            added_units: hints
                .iter()
                .filter(|hint| added.contains(&hint.unit_key))
                .map(|hint| markdown_unit_from_hint(hint, &request, source.as_mut()))
                .collect::<Result<_>>()?,
            removed_unit_keys: incremental
                .map(|hints| hints.removed_unit_keys)
                .unwrap_or_default(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
            // Deterministic local execution has no provider usage and does
            // not enter the billing ledger.
            usage: None,
        });
    }

    Ok(MarkdownizeResponse {
        mode_used: MarkdownizeMode::Full,
        updated_units: hints
            .iter()
            .map(|hint| markdown_unit_from_hint(hint, &request, source.as_mut()))
            .collect::<Result<_>>()?,
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: Vec::new(),
        fallback_to_full: false,
        reason: None,
        // QA17: see the Incremental branch above — this adapter never bills.
        usage: None,
    })
}

fn unit_kind_for_media_type(media_type: &str) -> UnitKind {
    match media_type {
        "application/pdf" => UnitKind::Page,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" => UnitKind::Image,
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => UnitKind::Sheet,
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            UnitKind::Slide
        }
        _ => UnitKind::File,
    }
}

fn default_hint(raw_hash: &str) -> PreparedUnitHint {
    PreparedUnitHint {
        unit_key: "doc:1".to_owned(),
        prepared_hash: raw_hash.to_owned(),
        unit_kind: UnitKind::File,
        order: 0,
    }
}

/// QA41/QA42 (step4b-contract-tests-p3a.md §L): the sentinel used when there is
/// nothing to extract for a unit (no source, or extracted text is empty/not
/// real text). Deliberately plain text, not an HTML comment — an HTML
/// comment is a raw-HTML block under Normalized Markdown v1 (07 §5.2.1) and
/// would be rejected by the same v1 structural check this baseline must
/// satisfy (04 §3.2 V5).
fn baseline_placeholder(unit_key: &str, prepared_hash: &str) -> String {
    format!("Kio deterministic baseline: {unit_key} {prepared_hash}\n")
}

fn markdown_unit_from_hint(
    hint: &PreparedUnitHint,
    request: &MarkdownizeRequest,
    source: Option<&mut SourceDocument<'_>>,
) -> Result<MarkdownUnit> {
    let markdown = match source {
        Some(SourceDocument::Text(text)) if request.media_type == "text/markdown" => {
            text.to_owned()
        }
        Some(SourceDocument::Pdf { pages, office_kind }) => {
            let index = page_index_from_unit_key(&hint.unit_key).unwrap_or(hint.order as usize);
            let page = pages.read_page(index)?;
            let auto_hint;
            let hint = if office_kind.is_some() && request.prepared_unit_hint.is_none() {
                auto_hint = PreparedUnitHint {
                    prepared_hash: crate::identity::hash_bytes(page.as_bytes()),
                    ..hint.clone()
                };
                &auto_hint
            } else {
                hint
            };
            let unit = finish_markdown_unit(hint, page_markdown(&page));
            pages.retain_output(unit.markdown.len())?;
            return Ok(unit);
        }
        Some(SourceDocument::Sheets(sheets)) => {
            let sheet = sheets.get(hint.order as usize).map_or("", String::as_str);
            format!("{}\n", sheet.trim())
        }
        // text/plain and sniffed octet-stream TEXT (R20-6 passthrough:
        // .xml/.html/.eml/.csv/...) — preserve it as literal text. Raw
        // passthrough put markup-bearing files (XML/HTML) in violation of
        // Normalized Markdown v1's "raw HTML and autolinks are forbidden"
        // acceptance check (04 §3.2 V5), which failed every such file's
        // offline markdownize at index time (found on the 2026-07-23
        // fixture registration: 48/48 failures were exactly the .xml and
        // .html corpus files). A fence is the V5-safe literal carrier for
        // ALL non-markdown text, independent of content.
        Some(SourceDocument::Text(text)) => fence_literal_text(text, request.raw.path.as_deref()),
        None => baseline_placeholder(&hint.unit_key, &hint.prepared_hash),
    };
    Ok(finish_markdown_unit(hint, markdown))
}

/// Format one verified prepared unit without decoding the original again.
pub fn markdownize_prepared_unit(
    hint: &PreparedUnitHint,
    media_type: &str,
    input_path: &str,
    bytes: &[u8],
) -> MarkdownUnit {
    let text = String::from_utf8_lossy(bytes);
    let markdown = match hint.unit_kind {
        UnitKind::Page | UnitKind::Slide => page_markdown(&text),
        UnitKind::Sheet => format!("{}\n", text.trim()),
        _ if media_type == "text/markdown" => {
            text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
        }
        _ => fence_literal_text(
            text.strip_prefix('\u{feff}').unwrap_or(&text),
            Some(input_path),
        ),
    };
    finish_markdown_unit(hint, markdown)
}

fn page_markdown(text: &str) -> String {
    let text = if is_probably_real_text(text) {
        text.trim()
    } else {
        ""
    };
    format!("{text}\n")
}

fn finish_markdown_unit(hint: &PreparedUnitHint, markdown: String) -> MarkdownUnit {
    let markdown = if markdown.trim().is_empty() {
        baseline_placeholder(&hint.unit_key, &hint.prepared_hash)
    } else {
        markdown
    };
    MarkdownUnit {
        unit_key: hint.unit_key.clone(),
        unit_type: hint.unit_kind,
        // QA42: decisive normalization to Normalized Markdown v1 (07 §5.2.1),
        // not passthrough — at minimum Setext -> ATX plus the encoding rules
        // (BOM/CRLF/NFC/trailing-space/final-LF), applied as the last step so
        // every branch above (raw passthrough, fenced code, PDF-extracted
        // text, the baseline sentinel) is covered uniformly.
        markdown: normalize_to_markdown_v1(&markdown),
        owned_image_hashes: Default::default(),
        metadata: Default::default(),
    }
}

/// QA42: decisive normalization to Normalized Markdown v1 (07 §5.2.1) —
/// BOM strip, CRLF/CR -> LF, Unicode NFC, per-line trailing-space strip
/// (applied inside a fenced code block too — encoding rules are not
/// fence-exempt, only *syntactic* transforms like the Setext rewrite are),
/// exactly one trailing LF, and Setext heading -> ATX heading conversion
/// (skipped inside a fenced code block, where a `---`/`===` line is data,
/// not a heading underline).
///
/// Shared with the offline OCR adapter rather than reimplemented there: every
/// adapter owes the same v1 output, and a second partial normalizer would drift
/// from this one. It deliberately does **not** touch raw HTML — that is a
/// content question each adapter answers for itself, and the acceptance check
/// still refuses what neither of them could bring into v1.
pub(crate) fn normalize_to_markdown_v1(text: &str) -> String {
    let no_bom = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lf_only = no_bom.replace("\r\n", "\n").replace('\r', "\n");
    let nfc: String = lf_only.nfc().collect();
    let source_lines: Vec<&str> = nfc.split('\n').collect();
    let mut lines: Vec<String> = Vec::with_capacity(source_lines.len());
    let mut in_fence = false;
    let mut index = 0;
    while index < source_lines.len() {
        let line = source_lines[index];
        if line.trim_start_matches(' ').starts_with("```") {
            in_fence = !in_fence;
            lines.push(strip_trailing_space(line));
            index += 1;
            continue;
        }
        if !in_fence
            && !line.trim().is_empty()
            && let Some(next) = source_lines.get(index + 1)
            && let Some(level) = setext_level(next)
        {
            lines.push(format!("{} {}", "#".repeat(level), line.trim()));
            index += 2;
            continue;
        }
        lines.push(strip_trailing_space(line));
        index += 1;
    }
    let mut result = lines.join("\n");
    while result.ends_with('\n') {
        result.pop();
    }
    result.push('\n');
    result
}

fn strip_trailing_space(line: &str) -> String {
    line.trim_end_matches([' ', '\t']).to_owned()
}

/// The Setext heading level (1 = `=`, 2 = `-`) for an underline candidate
/// line, or `None`. A `-` underline requires 2+ characters so a lone `-`
/// (ambiguous with other single-dash usage) is never reclassified.
fn setext_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start_matches(' ').trim_end_matches([' ', '\t']);
    if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte == b'=') {
        Some(1)
    } else if trimmed.len() >= 2 && trimmed.bytes().all(|byte| byte == b'-') {
        Some(2)
    } else {
        None
    }
}

/// R21-5: whether extracted PDF page text is REAL text vs binary garbage lossy-decoded
/// from a scanned page's compressed stream. Real text is overwhelmingly printable; garbage
/// is dense with U+FFFD replacement characters and control bytes. Mirrors the pipeline's
/// `prepare::is_probably_real_text` (kept local to avoid a kio-pipeline dependency).
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

fn fence_literal_text(text: &str, path: Option<&str>) -> String {
    let lang = path
        .and_then(|path| std::path::Path::new(path).extension())
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    // CommonMark: the fence must be strictly longer than any backtick run
    // the content itself starts a line with, or an embedded ``` would close
    // the fence early and leak the remainder as raw markdown (the same V5
    // rejection this fencing exists to prevent).
    let longest_run = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            trimmed.bytes().take_while(|byte| *byte == b'`').count()
        })
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}{lang}\n{}\n{fence}\n", text.trim_end())
}

/// The 0-based page index a `page:N` OR `slide:N` unit key refers to (both
/// prefixes share the same 1-based-decimal selector convention — 04 §2's
/// unit_key rule — so both resolve identically here; the `slide:` case
/// exists for [`markdownize_converted_office`]'s PPTX units, which reuse
/// this exact PDF-page-text lookup instead of forking it).
fn page_index_from_unit_key(unit_key: &str) -> Option<usize> {
    let selector = unit_key
        .strip_prefix("page:")
        .or_else(|| unit_key.strip_prefix("slide:"))?;
    selector
        .parse::<usize>()
        .ok()
        .and_then(|page| page.checked_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-23 fixture-registration regression: sniffed octet-stream TEXT
    /// (XML/HTML/...) must come out FENCED — raw passthrough tripped the
    /// Normalized Markdown v1 "raw HTML forbidden" acceptance check.
    #[test]
    fn octet_text_markdownize_is_fenced() {
        let text = "<record><title>x</title></record>";
        let fenced = fence_literal_text(text, Some("a/record-037.xml"));
        assert!(fenced.starts_with("```xml\n<record>"), "{fenced}");
        assert!(fenced.trim_end().ends_with("```"), "{fenced}");
    }

    /// CommonMark fence-length rule: content with its own ``` runs needs a
    /// LONGER fence, or the embedded run closes the block early and the
    /// remainder leaks as raw markdown.
    #[test]
    fn fence_grows_past_embedded_backtick_runs() {
        let text = "line\n````\ninner\n````";
        let fenced = fence_literal_text(text, None);
        assert!(fenced.starts_with("`````\n"), "{fenced}");
        assert!(fenced.trim_end().ends_with("`````"), "{fenced}");
    }

    use crate::traits::MarkdownizeAdapter;

    // R21-5: a mixed PDF's real text page passes `is_probably_real_text`; a scanned page's
    // lossy-decoded binary (dense with U+FFFD / control bytes) does not. Page selection
    // uses this to suppress the garbage page so it is never persisted as a searchable chunk,
    // while the R20-4 all-pages-garbage gate keeps routing a wholly-scanned doc to OCR.
    #[test]
    fn r21_5_is_probably_real_text_rejects_binary_garbage_page() {
        assert!(is_probably_real_text(
            "This is a genuine first page paragraph with real readable words."
        ));
        assert!(is_probably_real_text(
            "日本語のテキストレイヤーも本物として扱う。"
        ));
        let garbage: String = (0u32..600)
            .map(|i| {
                if i % 2 == 0 {
                    '\u{fffd}'
                } else {
                    char::from_u32(i % 0x1f + 1).unwrap_or('\u{1}')
                }
            })
            .collect();
        assert!(!is_probably_real_text(&garbage));
        assert!(!is_probably_real_text(""));
    }

    // QA42 (step4b-contract-tests-p3a.md §L): the built-in deterministic
    // adapter converts a Setext H1/H2 to ATX (04 §3.2 V5 / 07 §5.2.1) instead
    // of a straight passthrough.
    #[test]
    fn qa42_setext_headings_are_converted_to_atx() {
        assert_eq!(
            normalize_to_markdown_v1("Title\n=====\n\nBody text\n"),
            "# Title\n\nBody text\n"
        );
        assert_eq!(
            normalize_to_markdown_v1("Subtitle\n--------\n"),
            "## Subtitle\n"
        );
        // A lone `-` is never reclassified (ambiguous with other single-dash
        // usage — the underline heuristic requires 2+ characters).
        assert_eq!(normalize_to_markdown_v1("Line\n-\n"), "Line\n-\n");
        // A `---`/`===` line inside a fenced code block is data, not a
        // heading underline.
        assert_eq!(
            normalize_to_markdown_v1("```\nTitle\n=====\n```\n"),
            "```\nTitle\n=====\n```\n"
        );
    }

    // QA41/QA42: the encoding-level Normalized Markdown v1 rules (07 §5.2.1)
    // are applied unconditionally — BOM strip, CRLF -> LF, NFC, per-line
    // trailing-space strip (including inside a fence — only syntactic
    // transforms like Setext are fence-exempt), and exactly one trailing LF.
    #[test]
    fn qa41_encoding_rules_are_normalized_even_inside_a_fence() {
        assert_eq!(
            normalize_to_markdown_v1("\u{feff}# Heading\r\n\r\nBody\r\n"),
            "# Heading\n\nBody\n"
        );
        assert_eq!(
            normalize_to_markdown_v1("Trailing space here   \nNext line\t\n"),
            "Trailing space here\nNext line\n"
        );
        assert_eq!(
            normalize_to_markdown_v1("```\nlet x = 1;   \n```"),
            "```\nlet x = 1;\n```\n"
        );
        // NFD combining sequence (e already knows the Step2a fixture: e +
        // U+0301) normalizes to the precomposed NFC form.
        assert_eq!(normalize_to_markdown_v1("cafe\u{0301}\n"), "caf\u{e9}\n");
        // Multiple trailing blank lines collapse to exactly one trailing LF.
        assert_eq!(normalize_to_markdown_v1("Body\n\n\n"), "Body\n");
    }

    // QA41: the offline baseline placeholder itself must satisfy Normalized
    // Markdown v1 (07 §5.2.1) — plain text, not an HTML comment (an HTML
    // comment is a raw-HTML block, forbidden by 04 §3.2 V5).
    #[test]
    fn qa41_baseline_placeholder_is_plain_text_not_raw_html() {
        let placeholder = baseline_placeholder("page:1", "sha256:abc");
        assert!(!placeholder.contains('<'));
        assert!(!placeholder.contains('>'));
        assert_eq!(normalize_to_markdown_v1(&placeholder), placeholder);
    }

    #[test]
    fn placeholder_deterministic_profile_disallows_network() {
        let adapter = DeterministicAdapter;
        let profile = MarkdownizeAdapter::profile(&adapter);

        assert!(!profile.allow_network);
        assert_eq!(profile.adapter_id, "deterministic_builtin");
    }

    // O4: a multibyte char straddling the fixed lookahead window from a `/Page`
    // or `/Type` token used to panic on the str slice's char boundary (aborting
    // `kio index` with exit 101 and dumping the body to stderr). It must now be
    // counted cleanly without panicking.
    #[test]
    fn o4_pdf_page_count_survives_multibyte_char_boundary() {
        // Prefixes such as `/PageX` are not structural page names.
        assert_eq!(
            PdfPages::open(
                "%PDF-1.4\n/PageXあ padding to extend length".as_bytes(),
                ResourceTier::Standard
            )
            .unwrap()
            .len(),
            0
        );
        // "あ" straddles the +32 byte window measured from "/Type" (no panic).
        let type_case = format!("/Type{}あ/Pages", "y".repeat(26));
        let _ = PdfPages::open(type_case.as_bytes(), ResourceTier::Standard);
        // Genuine /Pages still suppresses the count.
        assert!(
            PdfPages::open(b"%PDF-1.4\n/Type /Pages catalog", ResourceTier::Standard)
                .unwrap()
                .is_empty()
        );
    }

    // Q5: a leading UTF-8 BOM (Windows Notepad / Excel / PowerShell default) used
    // to sit in front of the first ATX heading's `#`, so the chunker dropped that
    // heading. Deterministic source decoding must strip one leading BOM so the produced
    // markdown starts at the heading.
    #[test]
    fn q5_leading_bom_does_not_hide_first_heading() {
        use crate::types::{MarkdownizeMode, MarkdownizeRequest, RawInput};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.md");
        let bytes = b"\xef\xbb\xbf# Heading One\n\nbody\n";
        std::fs::write(&path, bytes).unwrap();
        let request = MarkdownizeRequest {
            raw: RawInput {
                raw_hash: crate::identity::hash_bytes(bytes),
                path: Some(path.display().to_string()),
            },
            media_type: "text/markdown".to_owned(),
            prepared_unit_hint: None,
            mode: MarkdownizeMode::Full,
            previous: None,
            hints: None,
            restrict_to_hint_pages: false,
            bbox_annotation_enabled: false,
            tool_profile_hash: format!("sha256:{}", "1".repeat(64)),
            spec_version: 1,
            idempotency_token: None,
        };
        let response = MarkdownizeAdapter::markdownize(&DeterministicAdapter, request).unwrap();
        let markdown = &response.updated_units[0].markdown;
        assert!(
            markdown.starts_with("# Heading One"),
            "BOM must be stripped so the heading sits at column 0: {markdown:?}"
        );
        assert!(!markdown.contains('\u{feff}'), "no BOM should remain");
    }

    #[test]
    fn pdf_page_index_ignores_markers_inside_strings_and_streams() {
        let pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Pages /Count 1 >> endobj\n\
2 0 obj << /Type /Page /Parent 1 0 R /Note (/Type /Page /PageX) >>\n\
stream\nBT (/Type /Page and /PageX) Tj ET\nendstream\nendobj\n";
        let mut pages = PdfPages::open(pdf, ResourceTier::Standard).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages.read_page(0).unwrap(), "/Type /Page and /PageX");
    }

    #[test]
    fn verified_pdf_bytes_are_reused_for_all_hints() {
        use crate::types::{MarkdownizeMode, MarkdownizeRequest, RawInput};

        let pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Pages /Count 2 >> endobj\n\
2 0 obj << /Type /Page >> stream\nBT (First page) Tj ET\nendstream\nendobj\n\
3 0 obj << /Type /Page >> stream\nBT (Second page) Tj ET\nendstream\nendobj\n";
        let request = MarkdownizeRequest {
            raw: RawInput {
                raw_hash: crate::identity::hash_bytes(pdf),
                path: Some("/path/that/must/not/be/opened.pdf".to_owned()),
            },
            media_type: "application/pdf".to_owned(),
            prepared_unit_hint: Some(vec![
                PreparedUnitHint {
                    unit_key: "page:1".to_owned(),
                    prepared_hash: "sha256:first".to_owned(),
                    unit_kind: UnitKind::Page,
                    order: 0,
                },
                PreparedUnitHint {
                    unit_key: "page:2".to_owned(),
                    prepared_hash: "sha256:second".to_owned(),
                    unit_kind: UnitKind::Page,
                    order: 1,
                },
            ]),
            mode: MarkdownizeMode::Full,
            previous: None,
            hints: None,
            restrict_to_hint_pages: false,
            bbox_annotation_enabled: false,
            tool_profile_hash: "sha256:tool".to_owned(),
            spec_version: 1,
            idempotency_token: None,
        };
        let response = markdownize_from_bytes(request, pdf).unwrap();
        assert_eq!(response.updated_units.len(), 2);
        assert!(response.updated_units[0].markdown.contains("First page"));
        assert!(response.updated_units[1].markdown.contains("Second page"));
    }

    fn office_request(
        raw_hash: &str,
        unit_hint: Option<Vec<PreparedUnitHint>>,
    ) -> MarkdownizeRequest {
        use crate::types::RawInput;
        MarkdownizeRequest {
            raw: RawInput {
                raw_hash: raw_hash.to_owned(),
                // The ORIGINAL office file's path — never reopened by
                // markdownize_converted_office (it only reads converted_pdf).
                path: Some("/path/that/must/not/be/opened.docx".to_owned()),
            },
            media_type: "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                .to_owned(),
            prepared_unit_hint: unit_hint,
            mode: MarkdownizeMode::Full,
            previous: None,
            hints: None,
            restrict_to_hint_pages: false,
            bbox_annotation_enabled: false,
            tool_profile_hash: "sha256:tool".to_owned(),
            spec_version: 1,
            idempotency_token: None,
        }
    }

    // A converted-PDF's identity is unrelated to the ORIGINAL office file's
    // raw_hash (they are different artifacts entirely) — markdownize_converted_office
    // must not run the raw_hash-equality check markdownize_from_bytes does.
    #[test]
    fn markdownize_converted_office_does_not_check_raw_hash_against_converted_pdf() {
        let converted_pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Pages /Count 1 >> endobj\n\
2 0 obj << /Type /Page >> stream\nBT (Only page) Tj ET\nendstream\nendobj\n";
        let request = office_request("sha256:original-office-file-hash", None);
        let response =
            markdownize_converted_office(request, converted_pdf, UnitKind::Page).unwrap();
        assert_eq!(response.updated_units.len(), 1);
        assert!(response.updated_units[0].markdown.contains("Only page"));
    }

    // DOCX: UnitKind::Page mints page:N hints (1-based) with no caller-supplied
    // prepared_unit_hint, and reuses the existing PDF text-layer production
    // per page (not forked).
    #[test]
    fn markdownize_converted_office_docx_mints_page_keys_for_every_converted_page() {
        let converted_pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Pages /Count 2 >> endobj\n\
2 0 obj << /Type /Page >> stream\nBT (First converted page) Tj ET\nendstream\nendobj\n\
3 0 obj << /Type /Page >> stream\nBT (Second converted page) Tj ET\nendstream\nendobj\n";
        let request = office_request("sha256:docx-raw", None);
        let response =
            markdownize_converted_office(request, converted_pdf, UnitKind::Page).unwrap();
        assert_eq!(response.updated_units.len(), 2);
        assert_eq!(response.updated_units[0].unit_key, "page:1");
        assert_eq!(response.updated_units[0].unit_type, UnitKind::Page);
        assert!(
            response.updated_units[0]
                .markdown
                .contains("First converted page")
        );
        assert_eq!(response.updated_units[1].unit_key, "page:2");
        assert!(
            response.updated_units[1]
                .markdown
                .contains("Second converted page")
        );
    }

    // PPTX: UnitKind::Slide mints slide:N hints instead — same converted-PDF
    // text-layer machinery, only the unit-key family differs.
    #[test]
    fn markdownize_converted_office_pptx_mints_slide_keys_for_every_converted_page() {
        let converted_pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Pages /Count 2 >> endobj\n\
2 0 obj << /Type /Page >> stream\nBT (First slide) Tj ET\nendstream\nendobj\n\
3 0 obj << /Type /Page >> stream\nBT (Second slide) Tj ET\nendstream\nendobj\n";
        let request = office_request("sha256:pptx-raw", None);
        let response =
            markdownize_converted_office(request, converted_pdf, UnitKind::Slide).unwrap();
        assert_eq!(response.updated_units.len(), 2);
        assert_eq!(response.updated_units[0].unit_key, "slide:1");
        assert_eq!(response.updated_units[0].unit_type, UnitKind::Slide);
        assert!(response.updated_units[0].markdown.contains("First slide"));
        assert_eq!(response.updated_units[1].unit_key, "slide:2");
        assert!(response.updated_units[1].markdown.contains("Second slide"));
    }

    // When the caller DOES supply prepared_unit_hint (the expected real-pipeline
    // shape, derived from the actual PreparedUnit list), those hints are used
    // as-is rather than the page-count-derived fallback.
    #[test]
    fn markdownize_converted_office_honors_caller_supplied_hints() {
        let converted_pdf = b"%PDF-1.4\n\
1 0 obj << /Type /Pages /Count 2 >> endobj\n\
2 0 obj << /Type /Page >> stream\nBT (First slide) Tj ET\nendstream\nendobj\n\
3 0 obj << /Type /Page >> stream\nBT (Second slide) Tj ET\nendstream\nendobj\n";
        let hints = vec![
            PreparedUnitHint {
                unit_key: "slide:1".to_owned(),
                prepared_hash: "sha256:caller-page-1".to_owned(),
                unit_kind: UnitKind::Slide,
                order: 0,
            },
            PreparedUnitHint {
                unit_key: "slide:2".to_owned(),
                prepared_hash: "sha256:caller-page-2".to_owned(),
                unit_kind: UnitKind::Slide,
                order: 1,
            },
        ];
        let request = office_request("sha256:pptx-raw", Some(hints));
        let response =
            markdownize_converted_office(request, converted_pdf, UnitKind::Slide).unwrap();
        assert_eq!(response.updated_units.len(), 2);
        assert!(response.updated_units[0].markdown.contains("First slide"));
        assert!(response.updated_units[1].markdown.contains("Second slide"));
    }

    // page_index_from_unit_key (shared by both application/pdf and converted
    // Office markdownize) resolves the slide: prefix identically to page:.
    #[test]
    fn page_index_from_unit_key_resolves_both_page_and_slide_prefixes() {
        assert_eq!(page_index_from_unit_key("page:1"), Some(0));
        assert_eq!(page_index_from_unit_key("page:12"), Some(11));
        assert_eq!(page_index_from_unit_key("slide:1"), Some(0));
        assert_eq!(page_index_from_unit_key("slide:7"), Some(6));
        assert_eq!(page_index_from_unit_key("sheet:Sheet1"), None);
        assert_eq!(page_index_from_unit_key("doc:1"), None);
    }
}
