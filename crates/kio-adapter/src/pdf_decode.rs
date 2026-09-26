//! Bounded decoder for FlateDecode-compressed PDF text layers (07 §2.1).
//!
//! Real-world text-layer PDFs (TeX / LibreOffice output) do not carry their
//! text as raw `(...)` literals the way the bounded conservative scanner in
//! [`crate::deterministic`] expects: content streams are FlateDecode-
//! compressed, glyphs are shown as subset-font *glyph indices* in hex
//! strings, and the glyph→Unicode mapping lives in per-font ToUnicode CMaps
//! — which TeX Live additionally packs inside a compressed object stream
//! (`/Type /ObjStm`). Extracting their text therefore needs four bounded
//! steps, all implemented here with the same hand-rolled byte-walker
//! discipline as `deterministic.rs` (no external PDF parser):
//!
//! 1. index `N G obj … endobj` spans (expanding `/Type /ObjStm` containers),
//! 2. resolve the Page → Contents / Resources → Font → ToUnicode graph,
//! 3. parse each ToUnicode CMap (`bfchar` / `bfrange`, code width from the
//!    codespace range),
//! 4. tokenize each page's (inflated) content stream, tracking the current
//!    font through `Tf` and decoding `Tj`/`TJ`/`'`/`"` show operators.
//!
//! Fail-empty posture: malformed-but-benign PDF constructs return `None`; the
//! caller first tries its bounded conservative scanner, and routes to OCR only
//! when that scanner also has no text. Resource-limit and malformed
//! object-stream admission failures are hard `ContractViolation`s so callers
//! never silently route an attacker-controlled resource exhaustion attempt to
//! a less bounded path.

use std::collections::{HashMap, HashSet};

use crate::deterministic::{
    find_endstream_bytes, is_pdf_delimiter, pdf_keyword_at, pdf_name_at, skip_pdf_literal_string,
    skip_pdf_space_and_comments,
};
use crate::{AdapterError, Result};

/// Per-stream inflate ceiling. A legitimate text content stream or ToUnicode
/// CMap is kilobytes; 16 MiB leaves three orders of magnitude of headroom
/// while keeping a hostile deflate bomb from ballooning memory.
pub const MAX_INFLATED_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// Object-index ceiling (raw objects + ObjStm-expanded sub-objects).
const MAX_PDF_OBJECTS: usize = 8192;

/// Cumulative bytes copied into the object index. This is deliberately much
/// larger than any normal office PDF's object graph, but prevents a document
/// made of many individually-valid object streams from consuming unbounded
/// heap while being expanded.
const MAX_MATERIALIZED_OBJECT_BYTES: usize = 128 * 1024 * 1024;

/// Total unfiltered or inflated stream bytes admitted during one decode. A
/// per-stream ceiling alone permits an attacker to supply thousands of small
/// streams whose aggregate work and allocation are unbounded.
const MAX_DECODED_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// A page may reference many streams, but their concatenated decoded content
/// is bounded before it is copied into the tokenizer buffer.
const MAX_PAGE_CONTENT_BYTES: usize = 16 * 1024 * 1024;

/// Bound the text retained for all pages in one decode. This protects both
/// callers that retain every page and adversarial CMaps with expansive target
/// strings.
const MAX_EMITTED_TEXT_BYTES: usize = 32 * 1024 * 1024;

/// CMap expansion has a different shape from the compressed input: one short
/// range can materialize thousands of independently-owned Unicode strings.
/// Keep its retained mappings bounded across every font in the document.
const MAX_MATERIALIZED_CMAP_BYTES: usize = 32 * 1024 * 1024;
const MAX_MATERIALIZED_CMAP_ENTRIES: usize = 65_536;

/// A valid page needs very few content streams. This upper bound is checked
/// while parsing the page dictionary, before growing the reference vector.
const MAX_CONTENT_REFS_PER_PAGE: usize = 8192;

/// Per-CMap mapped-code ceiling (a full 2-byte code space).
const MAX_CMAP_ENTRIES: usize = 65_536;

/// TJ kern threshold (thousandths of an em, PDF text-space units): array
/// elements separated by a displacement at least this large are treated as a
/// word gap. Empirically TeX/LibreOffice inter-letter kerns are |v| ≤ ~35
/// and word gaps ≥ ~300.
const TJ_WORD_GAP_THRESHOLD: f64 = 100.0;

struct PdfObject {
    body: Vec<u8>,
}

/// Per-decode admission accounting. Every allocation based on untrusted PDF
/// structure is checked here before reserving or copying its bytes.
struct PdfDecodeBudget {
    materialized_object_bytes: usize,
    decoded_stream_bytes: usize,
    emitted_text_bytes: usize,
    materialized_cmap_bytes: usize,
    materialized_cmap_entries: usize,
}

impl PdfDecodeBudget {
    fn reserve(counter: &mut usize, amount: usize, limit: usize, label: &str) -> Result<()> {
        let next = counter.checked_add(amount).ok_or_else(|| {
            AdapterError::ContractViolation(format!("PDF {label} byte accounting overflow"))
        })?;
        if next > limit {
            return Err(AdapterError::ContractViolation(format!(
                "PDF {label} exceeds the {limit} byte ceiling"
            )));
        }
        *counter = next;
        Ok(())
    }

    fn reserve_materialized_object(&mut self, amount: usize) -> Result<()> {
        Self::reserve(
            &mut self.materialized_object_bytes,
            amount,
            MAX_MATERIALIZED_OBJECT_BYTES,
            "materialized object data",
        )
    }

    fn remaining_decoded_stream_bytes(&self) -> usize {
        MAX_DECODED_STREAM_BYTES - self.decoded_stream_bytes
    }

    fn reserve_decoded_stream(&mut self, amount: usize) -> Result<()> {
        Self::reserve(
            &mut self.decoded_stream_bytes,
            amount,
            MAX_DECODED_STREAM_BYTES,
            "decoded stream work",
        )
    }

    fn reserve_emitted_text(&mut self, amount: usize) -> Result<()> {
        Self::reserve(
            &mut self.emitted_text_bytes,
            amount,
            MAX_EMITTED_TEXT_BYTES,
            "emitted text",
        )
    }

    fn remaining_emitted_text_bytes(&self) -> usize {
        MAX_EMITTED_TEXT_BYTES - self.emitted_text_bytes
    }

    fn remaining_cmap_bytes(&self) -> usize {
        MAX_MATERIALIZED_CMAP_BYTES - self.materialized_cmap_bytes
    }

    fn reserve_cmap_entry(&mut self, bytes: usize) -> Result<()> {
        Self::reserve(
            &mut self.materialized_cmap_entries,
            1,
            MAX_MATERIALIZED_CMAP_ENTRIES,
            "materialized CMap entries",
        )?;
        Self::reserve(
            &mut self.materialized_cmap_bytes,
            bytes,
            MAX_MATERIALIZED_CMAP_BYTES,
            "materialized CMap data",
        )
    }
}

#[derive(Debug)]
struct CMap {
    code_bytes: usize,
    map: HashMap<u32, String>,
}

enum FontMap {
    /// Font resolved and its ToUnicode CMap parsed.
    Decoded(CMap),
    /// Font object exists but exposes no usable ToUnicode mapping: its show
    /// strings are glyph indices we cannot map. Emitting their lossy bytes
    /// would evidence garbage (R20-6 posture), so they decode to nothing.
    Opaque,
}

/// Decode the text pages of `bytes` through the object graph. `Ok(None)`
/// means "no confident decode" — caller may use its bounded conservative
/// scanner. `Ok(Some(pages))` always contains at least one non-empty page.
pub(crate) fn decode_pdf_pages(bytes: &[u8], max_pages: usize) -> Result<Option<Vec<String>>> {
    let mut budget = PdfDecodeBudget {
        materialized_object_bytes: 0,
        decoded_stream_bytes: 0,
        emitted_text_bytes: 0,
        materialized_cmap_bytes: 0,
        materialized_cmap_entries: 0,
    };
    let Some(objects) = collect_objects(bytes, &mut budget)? else {
        return Ok(None);
    };
    let pages = collect_page_numbers(&objects);
    if pages.is_empty() || pages.len() > max_pages {
        return Ok(None);
    }
    let fonts = global_font_maps(&objects, &mut budget)?;
    let mut out = Vec::with_capacity(pages.len());
    let mut any_text = false;
    for page_obj in &pages {
        let text = decode_page(&objects, *page_obj, &fonts, &mut budget)?;
        any_text |= !text.trim().is_empty();
        out.push(text);
    }
    Ok(any_text.then_some(out))
}

/// Whether a compressed (FlateDecode) PDF decodes to real text through the
/// object graph. Used by the pipeline's text-layer gate for PDFs whose raw
/// bytes carry no literal `BT`. Never errors: a bomb or malformed file is
/// simply "no text layer" here and stays on its existing OCR routing.
#[must_use]
pub fn pdf_compressed_text_probe(bytes: &[u8]) -> bool {
    if !contains_token(bytes, b"FlateDecode") {
        return false;
    }
    matches!(
        decode_pdf_pages(bytes, crate::deterministic::MAX_DETERMINISTIC_PDF_PAGES),
        Ok(Some(_))
    )
}

fn contains_token(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Step 1: object index
// ---------------------------------------------------------------------------

fn collect_objects(
    bytes: &[u8],
    budget: &mut PdfDecodeBudget,
) -> Result<Option<HashMap<u32, PdfObject>>> {
    let mut objects: HashMap<u32, PdfObject> = HashMap::new();
    let mut index = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => index = skip_pdf_comment(bytes, index),
            b'(' => index = skip_pdf_literal_string(bytes, index),
            b'<' | b'>' => index += 1,
            b's' if pdf_keyword_at(bytes, index, b"stream") => {
                index = find_endstream_bytes(bytes, index + b"stream".len()).unwrap_or(bytes.len());
            }
            b'o' if pdf_keyword_at(bytes, index, b"obj") => {
                let Some(number) = object_number_before(bytes, index) else {
                    index += b"obj".len();
                    continue;
                };
                let body_start = index + b"obj".len();
                let Some(body_end) = find_endobj(bytes, body_start) else {
                    return Ok(None);
                };
                let body = &bytes[body_start..body_end];
                // Incremental PDFs can redefine an indirect object. Preserve
                // the established last-definition-wins index semantics while
                // charging every copied definition against the aggregate.
                if !objects.contains_key(&number) && objects.len() >= MAX_PDF_OBJECTS {
                    return Err(AdapterError::ContractViolation(format!(
                        "PDF object index exceeds the {MAX_PDF_OBJECTS} object ceiling"
                    )));
                }
                budget.reserve_materialized_object(body.len())?;
                objects.insert(
                    number,
                    PdfObject {
                        body: body.to_vec(),
                    },
                );
                index = body_end + b"endobj".len();
            }
            _ => index += 1,
        }
    }
    if objects.is_empty() {
        return Ok(None);
    }
    expand_object_streams(&mut objects, budget)?;
    Ok(Some(objects))
}

fn skip_pdf_comment(bytes: &[u8], index: usize) -> usize {
    bytes[index..]
        .iter()
        .position(|byte| matches!(byte, b'\n' | b'\r'))
        .map(|offset| index + offset + 1)
        .unwrap_or(bytes.len())
}

/// Parse the `N G` integers immediately preceding an `obj` keyword.
fn object_number_before(bytes: &[u8], obj_index: usize) -> Option<u32> {
    let mut end = obj_index;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    // generation number
    let gen_start = digits_start(bytes, end)?;
    let mut num_end = gen_start;
    while num_end > 0 && bytes[num_end - 1].is_ascii_whitespace() {
        num_end -= 1;
    }
    let num_start = digits_start(bytes, num_end)?;
    std::str::from_utf8(&bytes[num_start..num_end])
        .ok()?
        .parse()
        .ok()
}

fn digits_start(bytes: &[u8], end: usize) -> Option<usize> {
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    (start < end).then_some(start)
}

/// Find the `endobj` keyword terminating an object body, skipping strings,
/// hex strings, comments, and stream payloads.
fn find_endobj(bytes: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => index = skip_pdf_comment(bytes, index),
            b'(' => index = skip_pdf_literal_string(bytes, index),
            b's' if pdf_keyword_at(bytes, index, b"stream") => {
                index = find_endstream_bytes(bytes, index + b"stream".len())?;
            }
            b'e' if pdf_keyword_at(bytes, index, b"endobj") => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// The first balanced `<< … >>` dictionary span inside an object body.
fn dict_span(body: &[u8]) -> Option<&[u8]> {
    let start = body.windows(2).position(|window| window == b"<<")?;
    let mut depth = 0_usize;
    let mut index = start;
    while index < body.len() {
        match body[index] {
            b'%' => index = skip_pdf_comment(body, index),
            b'(' => index = skip_pdf_literal_string(body, index),
            b'<' if body.get(index + 1) == Some(&b'<') => {
                depth += 1;
                index += 2;
            }
            b'<' => {
                // hex string: skip to closing single '>'
                index = body[index + 1..]
                    .iter()
                    .position(|byte| *byte == b'>')
                    .map(|offset| index + offset + 2)
                    .unwrap_or(body.len());
            }
            b'>' if body.get(index + 1) == Some(&b'>') => {
                depth = depth.saturating_sub(1);
                index += 2;
                if depth == 0 {
                    return Some(&body[start..index]);
                }
            }
            b's' if pdf_keyword_at(body, index, b"stream") => return Some(&body[start..index]),
            _ => index += 1,
        }
    }
    None
}

/// Raw stream payload of an object body (bytes between the `stream` line
/// break and the line-anchored `endstream`).
fn stream_payload(body: &[u8]) -> Option<&[u8]> {
    let mut index = 0_usize;
    let start = loop {
        if index >= body.len() {
            return None;
        }
        match body[index] {
            b'%' => index = skip_pdf_comment(body, index),
            b'(' => index = skip_pdf_literal_string(body, index),
            b's' if pdf_keyword_at(body, index, b"stream") => break index + b"stream".len(),
            _ => index += 1,
        }
    };
    let mut data_start = start;
    if body.get(data_start) == Some(&b'\r') {
        data_start += 1;
    }
    if body.get(data_start) == Some(&b'\n') {
        data_start += 1;
    }
    let end = find_endstream_bytes(body, data_start)?;
    let mut data_end = end - b"endstream".len();
    while data_end > data_start && matches!(body[data_end - 1], b'\n' | b'\r') {
        data_end -= 1;
    }
    Some(&body[data_start..data_end])
}

fn dict_has_pair(dict: &[u8], key: &[u8], value: &[u8]) -> bool {
    let mut index = 0_usize;
    while index < dict.len() {
        if dict[index] == b'/' && pdf_name_at(dict, index, key) {
            let next = skip_pdf_space_and_comments(dict, index + 1 + key.len());
            if pdf_name_at(dict, next, value) {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn dict_has_name(dict: &[u8], key: &[u8]) -> bool {
    (0..dict.len()).any(|index| dict[index] == b'/' && pdf_name_at(dict, index, key))
}

/// Parse `… /Key N G R …` — the indirect reference following a name key.
fn dict_ref(dict: &[u8], key: &[u8]) -> Option<u32> {
    let mut index = 0_usize;
    while index < dict.len() {
        if dict[index] == b'/' && pdf_name_at(dict, index, key) {
            let next = skip_pdf_space_and_comments(dict, index + 1 + key.len());
            return parse_ref(dict, next).map(|(number, _)| number);
        }
        index += 1;
    }
    None
}

fn dict_int(dict: &[u8], key: &[u8]) -> Option<usize> {
    let mut index = 0_usize;
    while index < dict.len() {
        if dict[index] == b'/' && pdf_name_at(dict, index, key) {
            let next = skip_pdf_space_and_comments(dict, index + 1 + key.len());
            let mut end = next;
            while dict.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            return std::str::from_utf8(&dict[next..end]).ok()?.parse().ok();
        }
        index += 1;
    }
    None
}

/// Parse an `N G R` reference at `index`; returns (object number, end index).
fn parse_ref(bytes: &[u8], index: usize) -> Option<(u32, usize)> {
    let mut end = index;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == index {
        return None;
    }
    let number: u32 = std::str::from_utf8(&bytes[index..end]).ok()?.parse().ok()?;
    let gen_start = skip_pdf_space_and_comments(bytes, end);
    let mut gen_end = gen_start;
    while bytes.get(gen_end).is_some_and(u8::is_ascii_digit) {
        gen_end += 1;
    }
    if gen_end == gen_start {
        return None;
    }
    let r_at = skip_pdf_space_and_comments(bytes, gen_end);
    if bytes.get(r_at) == Some(&b'R')
        && bytes
            .get(r_at + 1)
            .is_none_or(|byte| is_pdf_delimiter(*byte))
    {
        Some((number, r_at + 1))
    } else {
        None
    }
}

/// Whether `dict` declares FlateDecode as its (only) stream filter — either
/// `/Filter /FlateDecode` or `/Filter [ /FlateDecode ]`.
fn is_flate_filtered(dict: &[u8]) -> bool {
    let mut index = 0_usize;
    while index < dict.len() {
        if dict[index] == b'/' && pdf_name_at(dict, index, b"Filter") {
            let mut next = skip_pdf_space_and_comments(dict, index + 1 + b"Filter".len());
            if dict.get(next) == Some(&b'[') {
                next = skip_pdf_space_and_comments(dict, next + 1);
                if !pdf_name_at(dict, next, b"FlateDecode") {
                    return false;
                }
                let after = skip_pdf_space_and_comments(dict, next + 1 + b"FlateDecode".len());
                return dict.get(after) == Some(&b']');
            }
            return pdf_name_at(dict, next, b"FlateDecode");
        }
        index += 1;
    }
    false
}

fn has_any_filter(dict: &[u8]) -> bool {
    dict_has_name(dict, b"Filter")
}

/// Inflate a FlateDecode payload with the per-stream and caller admission
/// ceilings. A merely corrupt stream is `Ok(None)`.
fn inflate_bounded(data: &[u8], output_limit: usize) -> Result<Option<Vec<u8>>> {
    use miniz_oxide::inflate::TINFLStatus;
    use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit};
    let limit = MAX_INFLATED_STREAM_BYTES.min(output_limit);
    if limit == 0 {
        return Err(AdapterError::ContractViolation(
            "PDF decoded stream work exceeds the aggregate byte ceiling".to_owned(),
        ));
    }
    match decompress_to_vec_zlib_with_limit(data, limit) {
        Ok(out) => Ok(Some(out)),
        Err(error) if error.status == TINFLStatus::HasMoreOutput => {
            Err(AdapterError::ContractViolation(format!(
                "deterministic PDF stream inflates past the {MAX_INFLATED_STREAM_BYTES} byte ceiling"
            )))
        }
        // Not zlib-wrapped? Some writers emit raw deflate.
        Err(_) => match decompress_to_vec_with_limit(data, limit) {
            Ok(out) => Ok(Some(out)),
            Err(error) if error.status == TINFLStatus::HasMoreOutput => {
                Err(AdapterError::ContractViolation(format!(
                    "deterministic PDF stream inflates past the {MAX_INFLATED_STREAM_BYTES} byte ceiling"
                )))
            }
            Err(_) => Ok(None),
        },
    }
}

/// Effective (post-filter) bytes of an object's stream: raw when unfiltered,
/// inflated when FlateDecode, `None` for any other filter or corrupt data.
fn effective_stream(
    object: &PdfObject,
    budget: &mut PdfDecodeBudget,
    max_output_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let Some(payload) = stream_payload(&object.body) else {
        return Ok(None);
    };
    let Some(dict) = dict_span(&object.body) else {
        return Ok(None);
    };
    if is_flate_filtered(dict) {
        let data = inflate_bounded(
            payload,
            budget
                .remaining_decoded_stream_bytes()
                .min(max_output_bytes),
        )?;
        if let Some(ref data) = data {
            budget.reserve_decoded_stream(data.len())?;
        }
        Ok(data)
    } else if has_any_filter(dict) {
        Ok(None)
    } else {
        if payload.len() > max_output_bytes {
            return Err(AdapterError::ContractViolation(
                "PDF stream exceeds its admission byte ceiling".to_owned(),
            ));
        }
        budget.reserve_decoded_stream(payload.len())?;
        Ok(Some(payload.to_vec()))
    }
}

/// Expand `/Type /ObjStm` containers into their member objects (TeX Live
/// packs font and page dictionaries there).
fn expand_object_streams(
    objects: &mut HashMap<u32, PdfObject>,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let container_numbers: Vec<u32> = objects
        .iter()
        .filter(|(_, object)| {
            dict_span(&object.body).is_some_and(|dict| dict_has_pair(dict, b"Type", b"ObjStm"))
        })
        .map(|(number, _)| *number)
        .collect();
    for number in container_numbers {
        let (count, first, data) = {
            let object = &objects[&number];
            let Some(dict) = dict_span(&object.body) else {
                continue;
            };
            let (Some(count), Some(first)) = (dict_int(dict, b"N"), dict_int(dict, b"First"))
            else {
                continue;
            };
            let Some(data) = effective_stream(object, budget, MAX_INFLATED_STREAM_BYTES)? else {
                continue;
            };
            (count, first, data)
        };
        if count > MAX_PDF_OBJECTS {
            return Err(AdapterError::ContractViolation(format!(
                "PDF object stream declares {count} objects, exceeding the {MAX_PDF_OBJECTS} object ceiling"
            )));
        }
        if first > data.len() || count > first / 3 {
            return Err(AdapterError::ContractViolation(
                "PDF object stream has an impossible object header size".to_owned(),
            ));
        }
        // Header: `objnum offset` pairs (ascii ints) before `first`.
        let header = &data[..first];
        let mut numbers_offsets = Vec::with_capacity(count);
        let mut cursor = 0_usize;
        for _ in 0..count {
            let Some((objnum, next)) = parse_ascii_u32(header, cursor) else {
                return Err(AdapterError::ContractViolation(
                    "PDF object stream has fewer object-header pairs than /N declares".to_owned(),
                ));
            };
            let Some((offset, after)) = parse_ascii_usize(header, next) else {
                return Err(AdapterError::ContractViolation(
                    "PDF object stream has fewer object-header pairs than /N declares".to_owned(),
                ));
            };
            numbers_offsets.push((objnum, offset));
            cursor = after;
        }
        if !header[cursor..].iter().all(u8::is_ascii_whitespace) {
            return Err(AdapterError::ContractViolation(
                "PDF object stream header contains data beyond its declared /N pairs".to_owned(),
            ));
        }
        let body_len = data.len() - first;
        let mut seen_numbers = HashSet::with_capacity(count);
        let mut previous_offset = None;
        for (objnum, offset) in &numbers_offsets {
            if !seen_numbers.insert(*objnum) {
                return Err(AdapterError::ContractViolation(
                    "PDF object stream repeats an expanded object number".to_owned(),
                ));
            }
            if *offset >= body_len || previous_offset.is_some_and(|previous| *offset <= previous) {
                return Err(AdapterError::ContractViolation(
                    "PDF object stream offsets must be strictly increasing and in range".to_owned(),
                ));
            }
            previous_offset = Some(*offset);
        }
        let new_objects = numbers_offsets
            .iter()
            .filter(|(objnum, _)| !objects.contains_key(objnum))
            .count();
        if new_objects > MAX_PDF_OBJECTS - objects.len() {
            return Err(AdapterError::ContractViolation(format!(
                "PDF object stream expansion exceeds the {MAX_PDF_OBJECTS} object ceiling"
            )));
        }
        for (position, (objnum, offset)) in numbers_offsets.iter().enumerate() {
            let start = first.checked_add(*offset).ok_or_else(|| {
                AdapterError::ContractViolation("PDF object stream offset overflows".to_owned())
            })?;
            let end = numbers_offsets
                .get(position + 1)
                .map(|(_, next_offset)| {
                    first.checked_add(*next_offset).ok_or_else(|| {
                        AdapterError::ContractViolation(
                            "PDF object stream offset overflows".to_owned(),
                        )
                    })
                })
                .transpose()?
                .unwrap_or(data.len());
            let body = &data[start..end];
            budget.reserve_materialized_object(body.len())?;
            // Preserve raw-index semantics for an indirect object redefined
            // by an object stream; duplicates *within* this stream were
            // rejected before any member body was copied.
            objects.insert(
                *objnum,
                PdfObject {
                    body: body.to_vec(),
                },
            );
        }
    }
    Ok(())
}

fn parse_ascii_usize(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let start = skip_ascii_space(bytes, from);
    let mut end = start;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == start {
        return None;
    }
    let value = std::str::from_utf8(&bytes[start..end]).ok()?.parse().ok()?;
    Some((value, end))
}

fn parse_ascii_u32(bytes: &[u8], from: usize) -> Option<(u32, usize)> {
    let (value, end) = parse_ascii_usize(bytes, from)?;
    u32::try_from(value).ok().map(|value| (value, end))
}

fn skip_ascii_space(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

// ---------------------------------------------------------------------------
// Step 2: page graph and fonts
// ---------------------------------------------------------------------------

/// Page objects (`/Type /Page`), in ascending object-number order — the
/// emission order of every writer this decoder targets; the caller's
/// structural-page-count normalization stays authoritative for cardinality.
fn collect_page_numbers(objects: &HashMap<u32, PdfObject>) -> Vec<u32> {
    let mut pages: Vec<u32> = objects
        .iter()
        .filter(|(_, object)| {
            dict_span(&object.body).is_some_and(|dict| dict_has_pair(dict, b"Type", b"Page"))
        })
        .map(|(number, _)| *number)
        .collect();
    pages.sort_unstable();
    pages
}

/// Document-global font-name → mapping table, merged across every page's
/// `/Resources /Font` dictionary. A name bound to two DIFFERENT font objects
/// anywhere in the document is ambiguous and dropped (correct-or-empty).
fn global_font_maps(
    objects: &HashMap<u32, PdfObject>,
    budget: &mut PdfDecodeBudget,
) -> Result<HashMap<String, FontMap>> {
    let mut name_to_font_obj: HashMap<String, Option<u32>> = HashMap::new();
    for page_obj in collect_page_numbers(objects) {
        let Some(page) = objects.get(&page_obj) else {
            continue;
        };
        let Some(page_dict) = dict_span(&page.body) else {
            continue;
        };
        let resources_owned;
        let resources: &[u8] = if let Some(reference) = dict_ref(page_dict, b"Resources") {
            let Some(resource_obj) = objects.get(&reference) else {
                continue;
            };
            let Some(dict) = dict_span(&resource_obj.body) else {
                continue;
            };
            resources_owned = dict.to_vec();
            &resources_owned
        } else {
            page_dict
        };
        let font_dict_owned;
        let font_dict: &[u8] = if let Some(reference) = dict_ref(resources, b"Font") {
            let Some(font_obj) = objects.get(&reference) else {
                continue;
            };
            let Some(dict) = dict_span(&font_obj.body) else {
                continue;
            };
            font_dict_owned = dict.to_vec();
            &font_dict_owned
        } else {
            resources
        };
        for (name, number) in font_entries(font_dict) {
            name_to_font_obj
                .entry(name)
                .and_modify(|existing| {
                    if *existing != Some(number) {
                        *existing = None; // ambiguous across pages: drop
                    }
                })
                .or_insert(Some(number));
        }
    }
    let mut fonts = HashMap::new();
    for (name, number) in name_to_font_obj {
        let Some(number) = number else { continue };
        let map = match font_tounicode_cmap(objects, number, budget)? {
            Some(cmap) => FontMap::Decoded(cmap),
            None => FontMap::Opaque,
        };
        fonts.insert(name, map);
    }
    Ok(fonts)
}

/// `/Fname N G R` entries of a font-resource dictionary.
fn font_entries(dict: &[u8]) -> Vec<(String, u32)> {
    let mut entries = Vec::new();
    let mut index = 0_usize;
    while index < dict.len() {
        if dict[index] == b'/' {
            let name_end = pdf_name_end(dict, index + 1);
            let after = skip_pdf_space_and_comments(dict, name_end);
            if let Some((number, _)) = parse_ref(dict, after) {
                if let Ok(name) = std::str::from_utf8(&dict[index + 1..name_end]) {
                    // Skip structural keys that also precede refs.
                    if !matches!(
                        name,
                        "Resources" | "Contents" | "Parent" | "Font" | "ToUnicode"
                    ) {
                        entries.push((name.to_owned(), number));
                    }
                }
                index = after;
                continue;
            }
        }
        index += 1;
    }
    entries
}

fn pdf_name_end(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| !is_pdf_delimiter(*byte))
    {
        index += 1;
    }
    index
}

fn font_tounicode_cmap(
    objects: &HashMap<u32, PdfObject>,
    font_obj: u32,
    budget: &mut PdfDecodeBudget,
) -> Result<Option<CMap>> {
    let Some(font) = objects.get(&font_obj) else {
        return Ok(None);
    };
    let Some(dict) = dict_span(&font.body) else {
        return Ok(None);
    };
    let Some(reference) = dict_ref(dict, b"ToUnicode") else {
        return Ok(None);
    };
    let Some(cmap_obj) = objects.get(&reference) else {
        return Ok(None);
    };
    let Some(data) = effective_stream(cmap_obj, budget, MAX_INFLATED_STREAM_BYTES)? else {
        return Ok(None);
    };
    parse_tounicode_cmap(&data, budget)
}

// ---------------------------------------------------------------------------
// Step 3: ToUnicode CMap parsing
// ---------------------------------------------------------------------------

fn parse_tounicode_cmap(data: &[u8], budget: &mut PdfDecodeBudget) -> Result<Option<CMap>> {
    let text = String::from_utf8_lossy(data);
    let mut code_bytes = 0_usize;
    let mut map: HashMap<u32, String> = HashMap::new();

    // Code width from the first codespacerange entry.
    if let Some(start) = text.find("begincodespacerange") {
        let tail = &text[start + "begincodespacerange".len()..];
        if let Some((low, _)) = next_hex_token(tail) {
            code_bytes = (low.len() / 2).max(1);
        }
    }

    let mut section_from = 0_usize;
    while let Some(offset) = text[section_from..].find("beginbfchar") {
        let start = section_from + offset + "beginbfchar".len();
        let end = text[start..]
            .find("endbfchar")
            .map(|o| start + o)
            .unwrap_or(text.len());
        let mut rest = &text[start..end];
        while let Some((src, after_src)) = next_hex_token(rest) {
            let Some((dst, after_dst)) = next_hex_token(after_src) else {
                break;
            };
            if code_bytes == 0 {
                code_bytes = (src.len() / 2).max(1);
            }
            if let (Some(code), Some(target)) =
                (hex_to_code(src), cmap_target_from_hex(dst, budget)?)
            {
                if map.len() >= MAX_CMAP_ENTRIES {
                    return Err(AdapterError::ContractViolation(format!(
                        "PDF CMap exceeds the {MAX_CMAP_ENTRIES} entry ceiling"
                    )));
                }
                map.insert(code, target);
            }
            rest = after_dst;
        }
        section_from = end;
    }

    let mut section_from = 0_usize;
    while let Some(offset) = text[section_from..].find("beginbfrange") {
        let start = section_from + offset + "beginbfrange".len();
        let end = text[start..]
            .find("endbfrange")
            .map(|o| start + o)
            .unwrap_or(text.len());
        let mut rest = &text[start..end];
        while let Some((low_hex, after_low)) = next_hex_token(rest) {
            let Some((high_hex, after_high)) = next_hex_token(after_low) else {
                break;
            };
            if code_bytes == 0 {
                code_bytes = (low_hex.len() / 2).max(1);
            }
            let (Some(low), Some(high)) = (hex_to_code(low_hex), hex_to_code(high_hex)) else {
                break;
            };
            if low > high {
                return Err(AdapterError::ContractViolation(
                    "PDF CMap range has descending code bounds".to_owned(),
                ));
            }
            let trimmed = after_high.trim_start();
            if let Some(array_rest) = trimmed.strip_prefix('[') {
                // <lo> <hi> [ <d1> <d2> … ]
                let mut inner = array_rest;
                let mut code = low;
                while code <= high {
                    let Some((dst, after_dst)) = next_hex_token(inner) else {
                        break;
                    };
                    if let Some(target) = cmap_target_from_hex(dst, budget)? {
                        if map.len() >= MAX_CMAP_ENTRIES {
                            return Err(AdapterError::ContractViolation(format!(
                                "PDF CMap exceeds the {MAX_CMAP_ENTRIES} entry ceiling"
                            )));
                        }
                        map.insert(code, target);
                    }
                    inner = after_dst;
                    if code == high {
                        break;
                    }
                    code = code.checked_add(1).ok_or_else(|| {
                        AdapterError::ContractViolation("PDF CMap code range overflows".to_owned())
                    })?;
                }
                rest = inner
                    .find(']')
                    .map(|close| &inner[close + 1..])
                    .unwrap_or(inner);
            } else {
                let Some((dst_hex, after_dst)) = next_hex_token(after_high) else {
                    break;
                };
                let Some(mut units) = hex_to_utf16_units(dst_hex) else {
                    break;
                };
                let span = high - low;
                for step in 0..=span {
                    if map.len() >= MAX_CMAP_ENTRIES {
                        return Err(AdapterError::ContractViolation(format!(
                            "PDF CMap exceeds the {MAX_CMAP_ENTRIES} entry ceiling"
                        )));
                    }
                    let code = low.checked_add(step).ok_or_else(|| {
                        AdapterError::ContractViolation("PDF CMap code range overflows".to_owned())
                    })?;
                    map.insert(code, cmap_target_from_units(&units, budget)?);
                    if let Some(last) = units.last_mut() {
                        *last = last.wrapping_add(1);
                    }
                }
                rest = after_dst;
            }
        }
        section_from = end;
    }

    Ok(finish_cmap(code_bytes, map))
}

fn finish_cmap(code_bytes: usize, map: HashMap<u32, String>) -> Option<CMap> {
    if map.is_empty() {
        return None;
    }
    Some(CMap {
        code_bytes: code_bytes.clamp(1, 4),
        map,
    })
}

/// Next `<hex>` token in a CMap section; returns (hex digits, rest).
fn next_hex_token(text: &str) -> Option<(&str, &str)> {
    let open = text.find('<')?;
    let tail = &text[open + 1..];
    let close = tail.find('>')?;
    Some((&tail[..close], &tail[close + 1..]))
}

fn hex_to_code(hex: &str) -> Option<u32> {
    let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() || cleaned.len() > 8 {
        return None;
    }
    u32::from_str_radix(&cleaned, 16).ok()
}

fn hex_to_utf16_units(hex: &str) -> Option<Vec<u16>> {
    let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(4) {
        // A bare 2-digit destination is a raw byte value.
        if cleaned.len() == 2 {
            return u16::from_str_radix(&cleaned, 16)
                .ok()
                .map(|unit| vec![unit]);
        }
        return None;
    }
    (0..cleaned.len())
        .step_by(4)
        .map(|at| u16::from_str_radix(&cleaned[at..at + 4], 16).ok())
        .collect()
}

fn cmap_target_from_hex(hex: &str, budget: &mut PdfDecodeBudget) -> Result<Option<String>> {
    let Some(units) = hex_to_utf16_units(hex) else {
        return Ok(None);
    };
    cmap_target_from_units(&units, budget).map(Some)
}

fn cmap_target_from_units(units: &[u16], budget: &mut PdfDecodeBudget) -> Result<String> {
    // A lossy UTF-16 conversion can turn every unit into a three-byte U+FFFD.
    // Check this upper bound, and the next mapping slot, before materializing
    // the target string. The exact retained bytes are charged immediately
    // afterwards.
    let worst_case_bytes = units.len().checked_mul(3).ok_or_else(|| {
        AdapterError::ContractViolation("PDF CMap target byte accounting overflow".to_owned())
    })?;
    if budget.materialized_cmap_entries >= MAX_MATERIALIZED_CMAP_ENTRIES
        || worst_case_bytes > budget.remaining_cmap_bytes()
    {
        return Err(AdapterError::ContractViolation(format!(
            "PDF CMap expansion exceeds the {MAX_MATERIALIZED_CMAP_ENTRIES} entry or {MAX_MATERIALIZED_CMAP_BYTES} byte ceiling"
        )));
    }
    let target = String::from_utf16_lossy(units);
    budget.reserve_cmap_entry(target.len())?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// Step 4: content-stream decoding
// ---------------------------------------------------------------------------

fn decode_page(
    objects: &HashMap<u32, PdfObject>,
    page_obj: u32,
    fonts: &HashMap<String, FontMap>,
    budget: &mut PdfDecodeBudget,
) -> Result<String> {
    let Some(page) = objects.get(&page_obj) else {
        return Ok(String::new());
    };
    let Some(dict) = dict_span(&page.body) else {
        return Ok(String::new());
    };
    let mut content = Vec::new();
    let mut page_content_bytes = 0_usize;
    for reference in contents_refs(dict)? {
        let Some(object) = objects.get(&reference) else {
            continue;
        };
        let page_remaining = MAX_PAGE_CONTENT_BYTES
            .checked_sub(page_content_bytes)
            .and_then(|remaining| remaining.checked_sub(1))
            .ok_or_else(|| {
                AdapterError::ContractViolation(format!(
                    "PDF page content exceeds the {MAX_PAGE_CONTENT_BYTES} byte ceiling"
                ))
            })?;
        if let Some(data) = effective_stream(object, budget, page_remaining)? {
            page_content_bytes = page_content_bytes
                .checked_add(data.len())
                .and_then(|total| total.checked_add(1))
                .ok_or_else(|| {
                    AdapterError::ContractViolation(
                        "PDF page content byte accounting overflow".to_owned(),
                    )
                })?;
            if page_content_bytes > MAX_PAGE_CONTENT_BYTES {
                return Err(AdapterError::ContractViolation(format!(
                    "PDF page content exceeds the {MAX_PAGE_CONTENT_BYTES} byte ceiling"
                )));
            }
            content.try_reserve(data.len() + 1).map_err(|_| {
                AdapterError::ContractViolation(
                    "PDF page content allocation could not be reserved".to_owned(),
                )
            })?;
            content.extend_from_slice(&data);
            content.push(b'\n');
        }
    }
    if content.is_empty() {
        return Ok(String::new());
    }
    decode_content_ops(&content, fonts, budget)
}

/// `/Contents N G R` or `/Contents [N G R M G R …]`.
fn contents_refs(dict: &[u8]) -> Result<Vec<u32>> {
    let mut refs = Vec::new();
    let mut index = 0_usize;
    while index < dict.len() {
        if dict[index] == b'/' && pdf_name_at(dict, index, b"Contents") {
            let mut next = skip_pdf_space_and_comments(dict, index + 1 + b"Contents".len());
            if dict.get(next) == Some(&b'[') {
                next += 1;
                loop {
                    next = skip_pdf_space_and_comments(dict, next);
                    if dict.get(next) == Some(&b']') || next >= dict.len() {
                        break;
                    }
                    match parse_ref(dict, next) {
                        Some((number, after)) => {
                            if refs.len() >= MAX_CONTENT_REFS_PER_PAGE {
                                return Err(AdapterError::ContractViolation(format!(
                                    "PDF page declares more than {MAX_CONTENT_REFS_PER_PAGE} content references"
                                )));
                            }
                            refs.push(number);
                            next = after;
                        }
                        None => break,
                    }
                }
            } else if let Some((number, _)) = parse_ref(dict, next) {
                if refs.len() >= MAX_CONTENT_REFS_PER_PAGE {
                    return Err(AdapterError::ContractViolation(format!(
                        "PDF page declares more than {MAX_CONTENT_REFS_PER_PAGE} content references"
                    )));
                }
                refs.push(number);
            }
            return Ok(refs);
        }
        index += 1;
    }
    Ok(refs)
}

// A single TJ operand list normally has tens to thousands of items. Permit
// 262,144 operands and 16 MiB of representation (including enum slots), but
// charge cumulative admissions per content stream so nested/restarted or
// unterminated arrays cannot reset the work bound.
const MAX_CONTENT_OPERANDS: usize = 262_144;
const MAX_CONTENT_OPERAND_BYTES: usize = 16 * 1024 * 1024;

enum ShowString {
    Literal(Vec<u8>),
    Hex(Vec<u8>),
}

enum ArrayItem {
    Text(ShowString),
    Kern(f64),
}

/// Tokenize a content stream, tracking the current font through `Tf`, and
/// decode every show operator (`Tj`, `TJ`, `'`, `"`).
fn decode_content_ops(
    content: &[u8],
    fonts: &HashMap<String, FontMap>,
    budget: &mut PdfDecodeBudget,
) -> Result<String> {
    decode_content_ops_with_limits(
        content,
        fonts,
        budget,
        MAX_CONTENT_OPERANDS,
        MAX_CONTENT_OPERAND_BYTES,
    )
}

fn decode_content_ops_with_limits(
    content: &[u8],
    fonts: &HashMap<String, FontMap>,
    budget: &mut PdfDecodeBudget,
    operand_limit: usize,
    byte_limit: usize,
) -> Result<String> {
    let mut operands = 0;
    let mut operand_bytes = 0;
    let mut admit_operand = |payload: usize| -> Result<()> {
        PdfDecodeBudget::reserve(&mut operands, 1, operand_limit, "content operand count")?;
        let bytes = payload
            .checked_add(std::mem::size_of::<ArrayItem>())
            .ok_or_else(|| {
                AdapterError::ContractViolation(
                    "PDF content operand accounting overflow".to_owned(),
                )
            })?;
        PdfDecodeBudget::reserve(
            &mut operand_bytes,
            bytes,
            byte_limit,
            "content operand representation",
        )
    };
    let mut output = String::new();
    let mut index = 0_usize;
    let mut current_font: Option<&FontMap> = None;
    let mut last_name: Option<String> = None;
    let mut last_string: Option<ShowString> = None;
    let mut array_items: Option<Vec<ArrayItem>> = None;

    while index < content.len() {
        let byte = content[index];
        match byte {
            b'%' => index = skip_pdf_comment(content, index),
            b'(' => {
                let end = skip_pdf_literal_string(content, index);
                admit_operand(end.saturating_sub(index + 2))?;
                let literal = unescape_pdf_literal(&content[index + 1..end.saturating_sub(1)]);
                let string = ShowString::Literal(literal);
                match array_items.as_mut() {
                    Some(items) => items.push(ArrayItem::Text(string)),
                    None => last_string = Some(string),
                }
                index = end;
            }
            b'<' if content.get(index + 1) == Some(&b'<') => {
                // inline dictionary (BDC property lists): skip balanced
                let mut depth = 0_usize;
                while index < content.len() {
                    if content.get(index..index + 2) == Some(b"<<") {
                        depth += 1;
                        index += 2;
                    } else if content.get(index..index + 2) == Some(b">>") {
                        depth = depth.saturating_sub(1);
                        index += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'<' => {
                let end = content[index + 1..]
                    .iter()
                    .position(|b| *b == b'>')
                    .map(|offset| index + 1 + offset)
                    .unwrap_or(content.len());
                // Encoded length is a conservative bound for decoded storage.
                admit_operand(end - index - 1)?;
                let hex = hex_bytes(&content[index + 1..end]);
                let string = ShowString::Hex(hex);
                match array_items.as_mut() {
                    Some(items) => items.push(ArrayItem::Text(string)),
                    None => last_string = Some(string),
                }
                index = end + 1;
            }
            b'[' => {
                admit_operand(0)?;
                array_items = Some(Vec::new());
                index += 1;
            }
            b']' => index += 1,
            b'/' => {
                let end = pdf_name_end(content, index + 1);
                admit_operand(end - index - 1)?;
                last_name = std::str::from_utf8(&content[index + 1..end])
                    .ok()
                    .map(str::to_owned);
                index = end;
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => {
                let mut end = index + 1;
                while content
                    .get(end)
                    .is_some_and(|b| b.is_ascii_digit() || *b == b'.' || *b == b'-' || *b == b'+')
                {
                    end += 1;
                }
                if let Some(items) = array_items.as_mut()
                    && let Ok(value) = std::str::from_utf8(&content[index..end])
                        .unwrap_or("")
                        .parse::<f64>()
                {
                    admit_operand(0)?;
                    items.push(ArrayItem::Kern(value));
                }
                index = end;
            }
            _ if byte.is_ascii_alphabetic() || byte == b'\'' || byte == b'"' => {
                let end = if byte == b'\'' || byte == b'"' {
                    index + 1
                } else {
                    let mut end = index + 1;
                    while content.get(end).is_some_and(|b| {
                        b.is_ascii_alphabetic() || *b == b'*' || *b == b'0' || *b == b'1'
                    }) {
                        end += 1;
                    }
                    end
                };
                let op = &content[index..end];
                match op {
                    b"Tf" => {
                        current_font = last_name.as_deref().and_then(|name| fonts.get(name));
                    }
                    b"Tj" | b"'" | b"\"" => {
                        if let Some(string) = last_string.take() {
                            push_decoded(
                                &mut output,
                                decode_show_string(&string, current_font, budget)?,
                                budget,
                            )?;
                        }
                    }
                    b"TJ" => {
                        if let Some(items) = array_items.take() {
                            let mut assembled = String::new();
                            for item in items {
                                match item {
                                    ArrayItem::Text(string) => {
                                        if let Some(text) =
                                            decode_show_string(&string, current_font, budget)?
                                        {
                                            append_admitted_text(&mut assembled, &text, budget)?;
                                        }
                                    }
                                    ArrayItem::Kern(value) => {
                                        if value.abs() >= TJ_WORD_GAP_THRESHOLD
                                            && !assembled.ends_with(' ')
                                        {
                                            append_admitted_text(&mut assembled, " ", budget)?;
                                        }
                                    }
                                }
                            }
                            push_decoded(&mut output, Some(assembled), budget)?;
                        }
                    }
                    _ => {}
                }
                if array_items.is_some() && !matches!(op, b"TJ") {
                    // strings inside an unfinished array stay queued
                } else if !matches!(op, b"TJ") {
                    // ops other than TJ consume nothing further here
                }
                index = end;
            }
            _ => index += 1,
        }
    }
    Ok(output)
}

fn append_admitted_text(out: &mut String, text: &str, budget: &PdfDecodeBudget) -> Result<()> {
    let next = out.len().checked_add(text.len()).ok_or_else(|| {
        AdapterError::ContractViolation("PDF emitted text byte accounting overflow".to_owned())
    })?;
    if next > budget.remaining_emitted_text_bytes() {
        return Err(AdapterError::ContractViolation(format!(
            "PDF emitted text exceeds the {MAX_EMITTED_TEXT_BYTES} byte ceiling"
        )));
    }
    out.try_reserve(text.len()).map_err(|_| {
        AdapterError::ContractViolation(
            "PDF emitted text allocation could not be reserved".to_owned(),
        )
    })?;
    out.push_str(text);
    Ok(())
}

fn append_bounded_text(out: &mut String, text: &str, budget: &mut PdfDecodeBudget) -> Result<()> {
    budget.reserve_emitted_text(text.len())?;
    out.try_reserve(text.len()).map_err(|_| {
        AdapterError::ContractViolation(
            "PDF emitted text allocation could not be reserved".to_owned(),
        )
    })?;
    out.push_str(text);
    Ok(())
}

fn push_decoded(
    output: &mut String,
    text: Option<String>,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let Some(text) = text else { return Ok(()) };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    // Mirror the bounded scanner's literal filter: keep only strings carrying at least
    // one alphanumeric or non-ASCII character.
    if trimmed
        .chars()
        .any(|char| char.is_alphanumeric() || !char.is_ascii())
    {
        if !output.is_empty() {
            append_bounded_text(output, "\n", budget)?;
        }
        append_bounded_text(output, trimmed, budget)?;
    }
    Ok(())
}

fn decode_show_string(
    string: &ShowString,
    font: Option<&FontMap>,
    budget: &mut PdfDecodeBudget,
) -> Result<Option<String>> {
    let bytes = match string {
        ShowString::Literal(bytes) | ShowString::Hex(bytes) => bytes,
    };
    match font {
        Some(FontMap::Decoded(cmap)) => {
            let mut out = String::new();
            for chunk in bytes.chunks(cmap.code_bytes) {
                let mut code = 0_u32;
                for byte in chunk {
                    code = (code << 8) | u32::from(*byte);
                }
                if let Some(mapped) = cmap.map.get(&code) {
                    append_admitted_text(&mut out, mapped, budget)?;
                }
            }
            Ok(Some(out))
        }
        // A font we resolved but cannot map: glyph indices, not text.
        Some(FontMap::Opaque) => Ok(None),
        // No font context (minimal fixtures): literal bytes are the text.
        None => match string {
            ShowString::Literal(bytes) => {
                if let Ok(text) = std::str::from_utf8(bytes) {
                    if text.len() > budget.remaining_emitted_text_bytes() {
                        return Err(AdapterError::ContractViolation(format!(
                            "PDF emitted text exceeds the {MAX_EMITTED_TEXT_BYTES} byte ceiling"
                        )));
                    }
                    return Ok(Some(text.to_owned()));
                }
                // Lossy UTF-8 can expand each hostile byte into U+FFFD's
                // three-byte UTF-8 encoding. Admit that worst case before
                // asking it to allocate the owned replacement string.
                if bytes.len() > budget.remaining_emitted_text_bytes() / 3 {
                    return Err(AdapterError::ContractViolation(format!(
                        "PDF emitted text exceeds the {MAX_EMITTED_TEXT_BYTES} byte ceiling"
                    )));
                }
                let text = String::from_utf8_lossy(bytes);
                Ok(Some(text.into_owned()))
            }
            ShowString::Hex(_) => Ok(None),
        },
    }
}

fn hex_bytes(raw: &[u8]) -> Vec<u8> {
    let digits: Vec<u8> = raw.iter().copied().filter(u8::is_ascii_hexdigit).collect();
    let mut out = Vec::with_capacity(digits.len() / 2 + 1);
    let (pairs, remainder) = digits.as_chunks::<2>();
    for pair in pairs {
        let high = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
        let low = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push((high << 4) | low);
    }
    if let [odd] = remainder {
        let high = (*odd as char).to_digit(16).unwrap_or(0) as u8;
        out.push(high << 4);
    }
    out
}

/// Full PDF literal-string unescape: `\n \r \t \b \f \( \) \\`, 1–3 digit
/// octal escapes, and backslash-newline continuations.
fn unescape_pdf_literal(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'\\' {
            out.push(byte);
            index += 1;
            continue;
        }
        let Some(next) = bytes.get(index + 1) else {
            break;
        };
        index += 2;
        match next {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'(' => out.push(b'('),
            b')' => out.push(b')'),
            b'\\' => out.push(b'\\'),
            b'\r' => {
                if bytes.get(index) == Some(&b'\n') {
                    index += 1;
                }
            }
            b'\n' => {}
            b'0'..=b'7' => {
                let mut value = u32::from(next - b'0');
                for _ in 0..2 {
                    match bytes.get(index) {
                        Some(digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            index += 1;
                        }
                        _ => break,
                    }
                }
                out.push((value & 0xff) as u8);
            }
            other => out.push(*other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::deflate::compress_to_vec_zlib;

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

    /// LibreOffice-shaped fixture: 1-byte codes, bfchar CMap, Flate content.
    fn one_byte_cid_pdf() -> Vec<u8> {
        let content = zlib(b"BT\n/F1 10.5 Tf\n[<01>1<02>2<03>-420<0104>]TJ\nET\n");
        let cmap = zlib(
            b"/CIDInit/ProcSet findresource begin begincmap\n\
              1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
              4 beginbfchar\n<01> <0053>\n<02> <0079>\n<03> <006E0074>\n<04> <0021>\nendbfchar\n\
              endcmap end",
        );
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

    #[test]
    fn one_byte_bfchar_flate_content_decodes() {
        let pdf = one_byte_cid_pdf();
        let pages = decode_pdf_pages(&pdf, 16).expect("decode").expect("pages");
        // <01><02> = "Sy", <03> = "nt" (multi-unit target), kern -420 = word
        // gap, <01><04> = "S!".
        assert_eq!(pages, vec!["Synt S!".to_owned()]);
        assert!(pdf_compressed_text_probe(&pdf));
    }

    /// TeX-shaped fixture: 2-byte Identity-H codes, bfrange CMap, and the
    /// page + font dictionaries packed inside a compressed /Type /ObjStm.
    #[test]
    fn two_byte_bfrange_inside_objstm_decodes() {
        let content = zlib(b"BT\n/F7 9.9 Tf\n[<00240025>-375<0026>]TJ\nET\n");
        let cmap = zlib(
            b"begincmap\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
              1 beginbfrange\n<0024> <0026> <0041>\nendbfrange\nendcmap",
        );
        let member_page =
            b"<< /Type /Page /Contents 5 0 R /Resources << /Font << /F7 3 0 R >> >> >>";
        let member_font = b"<< /Type /Font /Encoding /Identity-H /ToUnicode 6 0 R >>";
        let mut packed = Vec::new();
        let header = format!("2 {} 3 {} ", 0, member_page.len() + 1);
        packed.extend_from_slice(member_page);
        packed.push(b'\n');
        packed.extend_from_slice(member_font);
        let first = header.len();
        let mut objstm_data = header.into_bytes();
        objstm_data.extend_from_slice(&packed);
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

        let pages = decode_pdf_pages(&pdf, 16).expect("decode").expect("pages");
        // <0024><0025> map to A,B; kern -375 = word gap; <0026> = C.
        assert_eq!(pages, vec!["AB C".to_owned()]);
        assert!(pdf_compressed_text_probe(&pdf));
    }

    /// 2026-07-23 fleet-OCR regression: TeX Live packs every /Type /Page
    /// dictionary (often beyond the linearized first page) inside a
    /// compressed /Type /ObjStm, so the RAW structural scan undercounts and
    /// `extract_pdf_text_pages_bounded` used to truncate the graph decoder's
    /// correct N pages down to that wrong count — silently dropping pages
    /// 2+ from the offline index. The Batch collect bijection ("OCR 3 pages
    /// vs requested 1") is what surfaced it. Pin: graph pages are the page
    /// authority end to end.
    #[test]
    fn multi_page_objstm_pages_survive_extract_without_truncation() {
        let members: Vec<(u32, Vec<u8>)> = (0..3)
            .map(|index| {
                (
                    11 + index,
                    format!("<< /Type /Page /Contents {} 0 R >>", 21 + index).into_bytes(),
                )
            })
            .collect();
        let mut header = String::new();
        let mut packed = Vec::new();
        let mut offsets = Vec::new();
        for (number, body) in &members {
            offsets.push((number, packed.len()));
            packed.extend_from_slice(body);
            packed.push(b'\n');
        }
        for (number, offset) in &offsets {
            header.push_str(&format!("{number} {offset} "));
        }
        let first = header.len();
        let mut objstm_data = header.into_bytes();
        objstm_data.extend_from_slice(&packed);
        let objstm = zlib(&objstm_data);

        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(stream_obj(
            1,
            &format!(
                "<< /Type /ObjStm /N 3 /First {first} /Length {} /Filter /FlateDecode >>",
                objstm.len()
            ),
            &objstm,
        ));
        for index in 0..3u32 {
            let text = format!("BT (objstm page {} body) Tj ET", index + 1);
            pdf.extend(stream_obj(
                21 + index,
                &format!("<< /Length {} >>", text.len()),
                text.as_bytes(),
            ));
        }
        pdf.extend_from_slice(b"%%EOF\n");

        let pages = decode_pdf_pages(&pdf, 16).expect("decode").expect("pages");
        assert_eq!(pages.len(), 3, "{pages:?}");
        // End to end through the extract entry (raw structural count here is
        // ZERO — the old normalization would have truncated to 1).
        let extracted =
            crate::deterministic::extract_pdf_text_pages_bounded(&pdf, 16).expect("extract");
        assert_eq!(extracted.len(), 3, "{extracted:?}");
        assert!(extracted[2].contains("objstm page 3 body"), "{extracted:?}");
    }

    #[test]
    fn image_only_flate_contents_do_not_decode_to_text() {
        let pixels = zlib(&[0xffu8; 4096]);
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(
            2,
            &format!(
                "<< /Subtype /Image /Width 64 /Height 64 /Length {} /Filter /FlateDecode >>",
                pixels.len()
            ),
            &pixels,
        ));
        pdf.extend_from_slice(b"%%EOF\n");
        assert!(decode_pdf_pages(&pdf, 16).expect("decode").is_none());
        assert!(!pdf_compressed_text_probe(&pdf));
    }

    #[test]
    fn inflate_bomb_is_a_contract_violation() {
        let bomb = zlib(&vec![0u8; MAX_INFLATED_STREAM_BYTES + 1]);
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(
            2,
            &format!("<< /Length {} /Filter /FlateDecode >>", bomb.len()),
            &bomb,
        ));
        pdf.extend_from_slice(b"%%EOF\n");
        let error = decode_pdf_pages(&pdf, 16).expect_err("bomb must error");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn corrupt_flate_stream_falls_back_to_none() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(
            2,
            "<< /Length 8 /Filter /FlateDecode >>",
            b"\x01\x02\x03\x04garbage",
        ));
        pdf.extend_from_slice(b"%%EOF\n");
        assert!(decode_pdf_pages(&pdf, 16).expect("decode").is_none());
        assert!(!pdf_compressed_text_probe(&pdf));
    }

    #[test]
    fn uncompressed_literal_content_still_decodes_via_graph() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(
            2,
            "<< /Length 26 >>",
            b"BT (plain fixture text) Tj ET",
        ));
        pdf.extend_from_slice(b"%%EOF\n");
        let pages = decode_pdf_pages(&pdf, 16).expect("decode").expect("pages");
        assert_eq!(pages, vec!["plain fixture text".to_owned()]);
    }

    #[test]
    fn opaque_font_without_tounicode_decodes_to_nothing() {
        let content = zlib(b"BT\n/F1 10 Tf\n<0102> Tj\nET\n");
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
        pdf.extend(obj(3, b"<< /Type /Font /Subtype /Type0 >>"));
        pdf.extend_from_slice(b"%%EOF\n");
        assert!(decode_pdf_pages(&pdf, 16).expect("decode").is_none());
    }

    #[test]
    fn literal_escapes_and_octal_unescape() {
        assert_eq!(
            unescape_pdf_literal(b"a\\(b\\)c\\\\d\\n\\101"),
            b"a(b)c\\d\nA".to_vec()
        );
    }

    #[test]
    fn compact_numeric_array_hits_production_operand_limit() {
        // About 512 KiB of input used to queue all operands without a semantic
        // bound. Keep this regression far below the 16 MiB stream ceiling.
        let content = format!("[{}", "0 ".repeat(MAX_CONTENT_OPERANDS));
        assert!(matches!(
            decode_content_ops(content.as_bytes(), &HashMap::new(), &mut test_budget()),
            Err(AdapterError::ContractViolation(_))
        ));
    }

    #[test]
    fn content_operand_limits_cover_unterminated_and_restarted_arrays() {
        let fonts = HashMap::new();
        for content in [
            b"[0 0".as_slice(),
            b"[0 [".as_slice(),
            b"[()()".as_slice(),
            b"[<><>".as_slice(),
        ] {
            assert!(
                decode_content_ops_with_limits(content, &fonts, &mut test_budget(), 3, 1024)
                    .is_ok()
            );
            assert!(matches!(
                decode_content_ops_with_limits(content, &fonts, &mut test_budget(), 2, 1024),
                Err(AdapterError::ContractViolation(_))
            ));
        }
    }

    #[test]
    fn content_operand_representation_exact_boundary_preserves_text() {
        let content = b"[(hello) -200 (world)] TJ";
        let bytes = 4 * std::mem::size_of::<ArrayItem>() + 10;
        assert_eq!(
            decode_content_ops_with_limits(content, &HashMap::new(), &mut test_budget(), 4, bytes)
                .unwrap(),
            "hello world"
        );
        assert!(matches!(
            decode_content_ops_with_limits(
                content,
                &HashMap::new(),
                &mut test_budget(),
                4,
                bytes - 1
            ),
            Err(AdapterError::ContractViolation(_))
        ));
    }

    fn test_budget() -> PdfDecodeBudget {
        PdfDecodeBudget {
            materialized_object_bytes: 0,
            decoded_stream_bytes: 0,
            emitted_text_bytes: 0,
            materialized_cmap_bytes: 0,
            materialized_cmap_entries: 0,
        }
    }

    #[test]
    fn short_bfrange_cannot_amplify_past_shared_cmap_budget() {
        let mut budget = test_budget();
        budget.materialized_cmap_entries = MAX_MATERIALIZED_CMAP_ENTRIES - 128;
        let cmap = b"1 beginbfrange\n<0000> <00FF> <0041>\nendbfrange";
        let error = parse_tounicode_cmap(cmap, &mut budget)
            .expect_err("a short range must not expand past the shared CMap budget");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn cmap_target_checks_worst_case_before_lossy_utf16_allocation() {
        let mut budget = test_budget();
        budget.materialized_cmap_bytes = MAX_MATERIALIZED_CMAP_BYTES - 2;
        let error = parse_tounicode_cmap(b"1 beginbfchar\n<01> <D800>\nendbfchar", &mut budget)
            .expect_err("lossy UTF-16 target must be admitted before allocation");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn incremental_object_redefinition_keeps_last_definition() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(2, "<< /Length 18 >>", b"BT (old content) Tj ET"));
        pdf.extend(obj(1, b"<< /Type /Page /Contents 3 0 R >>"));
        pdf.extend(stream_obj(3, "<< /Length 18 >>", b"BT (new content) Tj ET"));
        let pages = decode_pdf_pages(&pdf, 16).expect("decode").expect("pages");
        assert_eq!(pages, vec!["new content".to_owned()]);
    }

    #[test]
    fn object_stream_rejects_huge_declared_count_before_reserving() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(stream_obj(
            1,
            &format!("<< /Type /ObjStm /N {} /First 0 /Length 1 >>", usize::MAX),
            b"x",
        ));
        let error = decode_pdf_pages(&pdf, 16).expect_err("hostile /N must be rejected");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn object_stream_rejects_overlapping_or_descending_offsets() {
        for header in [&b"2 4 3 1 "[..], &b"2 0 3 0 "[..]] {
            let mut data = header.to_vec();
            data.extend_from_slice(b"abcdef");
            let mut pdf = b"%PDF-1.6\n".to_vec();
            pdf.extend(stream_obj(
                1,
                &format!(
                    "<< /Type /ObjStm /N 2 /First {} /Length {} >>",
                    header.len(),
                    data.len()
                ),
                &data,
            ));
            let error = decode_pdf_pages(&pdf, 16).expect_err("non-monotonic offsets must fail");
            assert!(matches!(error, AdapterError::ContractViolation(_)));
        }
    }

    #[test]
    fn repeated_content_reference_cannot_exceed_page_budget() {
        let payload = vec![b' '; 1024 * 1024];
        let refs = std::iter::repeat_n("2 0 R", 17)
            .collect::<Vec<_>>()
            .join(" ");
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(
            1,
            format!("<< /Type /Page /Contents [{refs}] >>").as_bytes(),
        ));
        pdf.extend(stream_obj(
            2,
            &format!("<< /Length {} >>", payload.len()),
            &payload,
        ));
        let error = decode_pdf_pages(&pdf, 16).expect_err("page aggregate must be bounded");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }
}
