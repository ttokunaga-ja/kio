//! Resource-tiered PDF text-layer decoder (07 §2.1, §5.1). Raw objects
//! borrow from the input. Object streams provide compressed dictionaries,
//! while content streams and ToUnicode mappings are decoded a page at a time.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::{AdapterError, Result};
use kio_core::error::{Resource, ResourceLimit, ResourceTier};

mod xref;

const MAX_PROBE_OBJECTS: usize = 8192;
const MAX_PROBE_DECODED_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROBE_TEXT_BYTES: usize = 1024 * 1024;

/// TJ kern threshold (thousandths of an em, PDF text-space units): array
/// elements separated by a displacement at least this large are treated as a
/// word gap. Empirically TeX/LibreOffice inter-letter kerns are |v| ≤ ~35
/// and word gaps ≥ ~300.
const TJ_WORD_GAP_THRESHOLD: f64 = 100.0;

struct PdfObject<'a> {
    body: Cow<'a, [u8]>,
    generation: u16,
    dictionary: Option<std::ops::Range<usize>>,
    stream: Option<std::ops::Range<usize>>,
}

impl<'a> PdfObject<'a> {
    fn new(body: Cow<'a, [u8]>, generation: u16, stream: Option<std::ops::Range<usize>>) -> Self {
        let start = skip_pdf_space_and_comments(&body, 0);
        let value = &body[start..];
        let dictionary = if value.starts_with(b"<<") {
            dict_span(value).map(|dict| start..start + dict.len())
        } else {
            None
        };
        Self {
            body,
            generation,
            dictionary,
            stream,
        }
    }

    fn dictionary(&self) -> Option<&[u8]> {
        self.body.get(self.dictionary.clone()?)
    }

    fn owned_bytes(&self) -> usize {
        match &self.body {
            Cow::Owned(body) => body.len(),
            Cow::Borrowed(_) => 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PdfRef {
    number: u32,
    generation: u16,
}

fn referenced_object<'a, 'pdf>(
    objects: &'a HashMap<u32, PdfObject<'pdf>>,
    reference: PdfRef,
) -> Option<&'a PdfObject<'pdf>> {
    objects
        .get(&reference.number)
        .filter(|object| object.generation == reference.generation)
}

/// Per-document work and peak retained parser data. The renderer probe also
/// has its own small limits and deadline because it verifies a single page.
struct PdfDecodeBudget {
    tier: ResourceTier,
    memory_limit: usize,
    work_limit: u64,
    text_limit: usize,
    retained: usize,
    work: u64,
    expanded_bytes: u64,
    deadline: Option<std::time::Instant>,
    #[cfg(test)]
    checks_before_timeout: Option<std::cell::Cell<usize>>,
    probe_decoded_bytes: usize,
}

impl Default for PdfDecodeBudget {
    fn default() -> Self {
        Self::new(ResourceTier::Standard)
    }
}

impl PdfDecodeBudget {
    fn new(tier: ResourceTier) -> Self {
        let (memory_mib, work_limit, text_mib) = match tier {
            ResourceTier::Standard => (256, 10_000_000, 16),
            ResourceTier::Large => (1024, 100_000_000, 64),
        };
        Self {
            tier,
            memory_limit: memory_mib * 1024 * 1024,
            work_limit,
            text_limit: text_mib * 1024 * 1024,
            retained: 0,
            work: 0,
            expanded_bytes: 0,
            deadline: None,
            #[cfg(test)]
            checks_before_timeout: None,
            probe_decoded_bytes: 0,
        }
    }

    fn check_deadline(&self) -> Result<()> {
        #[cfg(test)]
        if let Some(remaining) = &self.checks_before_timeout {
            let count = remaining.get();
            if count == 0 {
                return crate::office_convert::check_renderer_deadline(std::time::Instant::now());
            }
            remaining.set(count - 1);
        }
        if let Some(deadline) = self.deadline {
            crate::office_convert::check_renderer_deadline(deadline)?;
        }
        Ok(())
    }

    fn resource_limit(&self, resource: Resource, limit: u64, observed: u64) -> AdapterError {
        AdapterError::ResourceLimit(ResourceLimit {
            resource,
            stage: "extract".to_owned(),
            limit: Some(limit),
            observed: Some(observed),
            resource_tier: Some(self.tier),
        })
    }

    fn visit(&mut self, visits: usize) -> Result<()> {
        self.check_deadline()?;
        if self.deadline.is_some() {
            return Ok(());
        }
        let next = self
            .work
            .checked_add(visits as u64)
            .ok_or_else(|| self.resource_limit(Resource::Work, self.work_limit, u64::MAX))?;
        if next > self.work_limit {
            return Err(self.resource_limit(Resource::Work, self.work_limit, next));
        }
        self.work = next;
        Ok(())
    }

    fn scan(&mut self, bytes: usize) -> Result<()> {
        self.visit(bytes.div_ceil(1024))
    }

    fn expand(&mut self, bytes: usize) -> Result<()> {
        if self.deadline.is_some() {
            return Self::reserve(
                &mut self.probe_decoded_bytes,
                bytes,
                MAX_PROBE_DECODED_BYTES,
                "probe decoded stream",
            );
        }
        let next = self
            .expanded_bytes
            .checked_add(bytes as u64)
            .ok_or_else(|| self.resource_limit(Resource::Work, self.work_limit, u64::MAX))?;
        let visits = next.div_ceil(1024) - self.expanded_bytes.div_ceil(1024);
        self.visit(visits as usize)?;
        self.expanded_bytes = next;
        Ok(())
    }

    fn expansion_remaining(&self) -> usize {
        if self.deadline.is_some() {
            return MAX_PROBE_DECODED_BYTES - self.probe_decoded_bytes;
        }
        let available = (self.work_limit - self.work) * 1024;
        let partial = self.expanded_bytes.div_ceil(1024) * 1024 - self.expanded_bytes;
        usize::try_from(available + partial).unwrap_or(usize::MAX)
    }

    fn check_memory(&self, additional: usize) -> Result<()> {
        let observed = (self.retained as u64).checked_add(additional as u64);
        if observed.is_none_or(|bytes| bytes > self.memory_limit as u64) {
            if self.deadline.is_some() {
                return Err(AdapterError::ContractViolation(
                    "PDF probe exceeds its memory ceiling".to_owned(),
                ));
            }
            return Err(self.resource_limit(
                Resource::Memory,
                self.memory_limit as u64,
                observed.unwrap_or(u64::MAX),
            ));
        }
        Ok(())
    }

    fn retain(&mut self, additional: usize) -> Result<()> {
        self.check_memory(additional)?;
        self.retained += additional;
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        self.retained -= bytes;
    }

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

    fn reserve_decoded_step(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<()> {
        let mut length = output.len();
        Self::reserve(&mut length, bytes.len(), limit, "probe inflated stream")?;
        output.extend_from_slice(bytes);
        Ok(())
    }

    fn check_text(&self, next: usize) -> Result<()> {
        if next > self.text_limit {
            if self.deadline.is_some() {
                return Err(AdapterError::ContractViolation(
                    "PDF probe emitted text exceeds its byte ceiling".to_owned(),
                ));
            }
            return Err(self.resource_limit(
                Resource::UnitText,
                self.text_limit as u64,
                next as u64,
            ));
        }
        Ok(())
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

/// Indexed PDF source with a single document work budget. Page text and font
/// tables are built on demand and discarded between units.
pub struct PdfPages<'a> {
    objects: HashMap<u32, PdfObject<'a>>,
    pages: Vec<u32>,
    non_pdf_input: Option<&'a [u8]>,
    budget: PdfDecodeBudget,
}

impl<'a> PdfPages<'a> {
    pub fn open(bytes: &'a [u8], tier: ResourceTier) -> Result<Self> {
        let mut budget = PdfDecodeBudget::new(tier);
        if !bytes.starts_with(b"%PDF") {
            return Ok(Self {
                objects: HashMap::new(),
                pages: Vec::new(),
                non_pdf_input: Some(bytes),
                budget,
            });
        }
        let objects = collect_objects(bytes, &mut budget)?.unwrap_or_default();
        let pages = collect_page_numbers(&objects, &mut budget)?;
        Ok(Self {
            objects,
            pages,
            non_pdf_input: None,
            budget,
        })
    }

    pub fn len(&self) -> usize {
        self.pages.len() + usize::from(self.non_pdf_input.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read_page(&mut self, index: usize) -> Result<String> {
        if let Some(bytes) = self.non_pdf_input.filter(|_| index == 0) {
            return lossy_text(bytes, &self.budget, true);
        }
        let Some(&number) = self.pages.get(index) else {
            return Ok(String::new());
        };
        let retained = self.budget.retained;
        let result = decode_indexed_page(&self.objects, number, &mut self.budget);
        self.budget.retained = retained;
        result
    }

    /// Account for the result of an API that returns owned output in memory.
    pub(crate) fn retain_output(&mut self, bytes: usize) -> Result<()> {
        self.budget.retain(bytes)
    }
}

fn decode_indexed_page(
    objects: &HashMap<u32, PdfObject<'_>>,
    number: u32,
    budget: &mut PdfDecodeBudget,
) -> Result<String> {
    let fonts = page_font_maps(objects, number, budget)?;
    decode_page(objects, number, &fonts, budget)
}

/// Decode the page reached from the active trailer's catalog in a one-page
/// renderer probe PDF, using the same budgets as the offline PDF decoder.
pub(crate) fn office_probe_page_text(
    bytes: &[u8],
    deadline: std::time::Instant,
) -> Result<Option<String>> {
    if bytes.len() > crate::office_convert::MAX_OFFICE_PROBE_PDF_BYTES {
        return Err(AdapterError::ContractViolation(
            "Office probe PDF exceeds its byte limit".to_owned(),
        ));
    }
    let mut budget = PdfDecodeBudget {
        deadline: Some(deadline),
        text_limit: MAX_PROBE_TEXT_BYTES,
        ..PdfDecodeBudget::default()
    };
    budget.check_deadline()?;
    let (root, objects) = xref::probe_objects(bytes, &mut budget)?;
    let Some(page) = probe_page_reference(&objects, root, &budget)? else {
        return Ok(None);
    };
    let text = decode_indexed_page(&objects, page, &mut budget)?;
    budget.check_deadline()?;
    Ok(Some(text))
}

fn probe_page_reference(
    objects: &HashMap<u32, PdfObject<'_>>,
    root: PdfRef,
    budget: &PdfDecodeBudget,
) -> Result<Option<u32>> {
    let dict = |reference| referenced_object(objects, reference).and_then(PdfObject::dictionary);
    let Some(catalog) = dict(root).filter(|dict| probe_dict_type(dict, b"Catalog")) else {
        return Ok(None);
    };
    let Some(pages) = probe_dict_ref(catalog, b"Pages") else {
        return Ok(None);
    };
    if !dict(pages).is_some_and(|dict| probe_dict_type(dict, b"Pages")) {
        return Ok(None);
    }
    let mut pending = vec![pages];
    let mut visited = std::collections::HashSet::new();
    let mut page = None;
    while let Some(id) = pending.pop() {
        budget.check_deadline()?;
        if !visited.insert(id) {
            return Ok(None);
        }
        let Some(node) = dict(id) else {
            return Ok(None);
        };
        if probe_dict_type(node, b"Page") {
            if page.replace(id.number).is_some() {
                return Ok(None);
            }
            continue;
        }
        if !probe_dict_type(node, b"Pages") {
            return Ok(None);
        }
        let Some(node) = probe_dict_value(node, b"Kids") else {
            return Ok(None);
        };
        if node.first() != Some(&b'[') {
            return Ok(None);
        }
        let mut child = skip_pdf_space_and_comments(node, 1);
        while node.get(child) != Some(&b']') {
            budget.check_deadline()?;
            if pending.len() + visited.len() >= objects.len() {
                return Ok(None);
            }
            let Some((reference, after)) = parse_ref(node, child) else {
                return Ok(None);
            };
            pending.push(reference);
            child = skip_pdf_space_and_comments(node, after);
        }
    }
    Ok(page)
}

fn probe_dict_type(dict: &[u8], name: &[u8]) -> bool {
    probe_dict_value(dict, b"Type").is_some_and(|value| pdf_name_at(value, 0, name))
}

fn probe_dict_ref(dict: &[u8], key: &[u8]) -> Option<PdfRef> {
    parse_ref(probe_dict_value(dict, key)?, 0).map(|(number, _)| number)
}

/// Locate a field on the dictionary itself. Names inside nested values,
/// literals and comments do not name fields of the enclosing dictionary.
fn probe_dict_value<'a>(dict: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    dict.strip_prefix(b"<<")?;
    let mut index = 2;
    let mut depth = 1usize;
    while index < dict.len() {
        match dict[index] {
            b'%' => index = skip_pdf_comment(dict, index),
            b'(' => index = skip_pdf_literal_string(dict, index),
            b'<' if dict.get(index + 1) == Some(&b'<') => {
                depth += 1;
                index += 2;
            }
            b'<' => {
                index += dict[index + 1..].iter().position(|byte| *byte == b'>')? + 2;
            }
            b'>' if dict.get(index + 1) == Some(&b'>') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    break;
                }
                index += 2;
            }
            b'[' => {
                depth += 1;
                index += 1;
            }
            b']' => {
                depth = depth.checked_sub(1)?;
                index += 1;
            }
            b'/' if depth == 1 && pdf_name_at(dict, index, key) => {
                let start = skip_pdf_space_and_comments(dict, index + 1 + key.len());
                return Some(&dict[start..]);
            }
            _ => index += 1,
        }
    }
    None
}

fn collect_objects<'a>(
    bytes: &'a [u8],
    budget: &mut PdfDecodeBudget,
) -> Result<Option<HashMap<u32, PdfObject<'a>>>> {
    budget.scan(bytes.len())?;
    let mut objects = HashMap::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => index = skip_pdf_comment(bytes, index),
            b'(' => index = skip_pdf_literal_string(bytes, index),
            b's' if pdf_keyword_at(bytes, index, b"stream") => {
                index = find_endstream_bytes(bytes, index + b"stream".len())
                    .ok_or_else(invalid_stream)?;
            }
            b'o' if pdf_keyword_at(bytes, index, b"obj") => {
                let Some(reference) = object_reference_before(bytes, index) else {
                    index += b"obj".len();
                    continue;
                };
                budget.visit(1)?;
                let start = index + b"obj".len();
                let (end, stream) = raw_object_end(bytes, start)?;
                insert_object(
                    &mut objects,
                    reference.number,
                    PdfObject::new(
                        Cow::Borrowed(&bytes[start..end]),
                        reference.generation,
                        stream.map(|range| range.start - start..range.end - start),
                    ),
                    budget,
                )?;
                index = end + b"endobj".len();
            }
            _ => index += 1,
        }
    }
    if !expand_object_streams(&mut objects, budget)? {
        return Ok(None);
    }
    Ok(Some(objects))
}

fn insert_object<'a>(
    objects: &mut HashMap<u32, PdfObject<'a>>,
    number: u32,
    object: PdfObject<'a>,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let old_bytes = objects.get(&number).map_or(0, PdfObject::owned_bytes);
    let new_bytes = object.owned_bytes();
    if !objects.contains_key(&number) {
        budget.retain(std::mem::size_of::<(u32, PdfObject<'_>)>())?;
    }
    budget.release(old_bytes);
    budget.retain(new_bytes)?;
    objects.insert(number, object);
    Ok(())
}

fn invalid_stream() -> AdapterError {
    AdapterError::ContractViolation(
        "PDF object or stream has an invalid offset, length or terminator".to_owned(),
    )
}

fn skip_pdf_literal_string(bytes: &[u8], start: usize) -> usize {
    skip_pdf_literal_string_checked(bytes, start, |_| Ok::<(), std::convert::Infallible>(()))
        .expect("infallible scan")
}

fn skip_pdf_literal_string_checked<E>(
    bytes: &[u8],
    start: usize,
    mut check: impl FnMut(usize) -> std::result::Result<(), E>,
) -> std::result::Result<usize, E> {
    let mut index = start + 1;
    let mut depth = 1_usize;
    while index < bytes.len() {
        check(index)?;
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    break;
                }
            }
            _ => index += 1,
        }
    }
    Ok(index)
}

fn skip_pdf_space_and_comments(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            index += 1;
        }
        if bytes.get(index) != Some(&b'%') {
            return index;
        }
        index = bytes[index..]
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
            .map(|offset| index + offset + 1)
            .unwrap_or(bytes.len());
    }
}

fn pdf_name_at(bytes: &[u8], index: usize, name: &[u8]) -> bool {
    bytes.get(index) == Some(&b'/')
        && bytes.get(index + 1..index + 1 + name.len()) == Some(name)
        && bytes
            .get(index + 1 + name.len())
            .is_none_or(|byte| is_pdf_delimiter(*byte))
}

fn pdf_keyword_at(bytes: &[u8], index: usize, keyword: &[u8]) -> bool {
    bytes.get(index..index + keyword.len()) == Some(keyword)
        && (index == 0 || is_pdf_delimiter(bytes[index - 1]))
        && bytes
            .get(index + keyword.len())
            .is_none_or(|byte| is_pdf_delimiter(*byte))
}

fn is_pdf_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn find_endstream_bytes(bytes: &[u8], from: usize) -> Option<usize> {
    const TOKEN: &[u8] = b"endstream";
    let mut index = from;
    while index + TOKEN.len() <= bytes.len() {
        if bytes.get(index..index + TOKEN.len()) == Some(TOKEN)
            && (index == 0 || matches!(bytes[index - 1], b'\n' | b'\r'))
            && bytes
                .get(index + TOKEN.len())
                .is_none_or(u8::is_ascii_whitespace)
        {
            return Some(index + TOKEN.len());
        }
        index += 1;
    }
    None
}

fn skip_pdf_comment(bytes: &[u8], index: usize) -> usize {
    bytes[index..]
        .iter()
        .position(|byte| matches!(byte, b'\n' | b'\r'))
        .map(|offset| index + offset + 1)
        .unwrap_or(bytes.len())
}

/// Parse the `N G` integers immediately preceding an `obj` keyword.
fn object_reference_before(bytes: &[u8], obj_index: usize) -> Option<PdfRef> {
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
    Some(PdfRef {
        number: std::str::from_utf8(&bytes[num_start..num_end])
            .ok()?
            .parse()
            .ok()?,
        generation: std::str::from_utf8(&bytes[gen_start..end])
            .ok()?
            .parse()
            .ok()?,
    })
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
            _ => index += 1,
        }
    }
    None
}

/// Index raw stream ranges. A direct length takes precedence over delimiter
/// matches inside binary data. Indirect lengths are checked after indexing.
fn raw_object_end(bytes: &[u8], start: usize) -> Result<(usize, Option<std::ops::Range<usize>>)> {
    let value = skip_pdf_space_and_comments(bytes, start);
    let dictionary = if bytes[value..].starts_with(b"<<") {
        Some(dict_span(&bytes[value..]).ok_or_else(invalid_stream)?)
    } else {
        None
    };
    let Some(dict) = dictionary else {
        return Ok((find_endobj(bytes, start).ok_or_else(invalid_stream)?, None));
    };
    let keyword = skip_pdf_space_and_comments(bytes, value + dict.len());
    if !pdf_keyword_at(bytes, keyword, b"stream") {
        if !pdf_keyword_at(bytes, keyword, b"endobj") {
            return Err(invalid_stream());
        }
        return Ok((keyword, None));
    }
    let after = keyword + b"stream".len();
    let data_start = match bytes.get(after..) {
        Some(rest) if rest.starts_with(b"\r\n") => after + 2,
        Some(rest) if rest.starts_with(b"\n") || rest.starts_with(b"\r") => after + 1,
        _ => return Err(invalid_stream()),
    };
    let direct_length = if probe_dict_ref(dict, b"Length").is_some() {
        None
    } else {
        probe_dict_value(dict, b"Length")
            .map(|_| dict_int(dict, b"Length").ok_or_else(invalid_stream))
            .transpose()?
    };
    let (data_end, next) = if let Some(length) = direct_length {
        let data_end = data_start
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid_stream)?;
        let terminator = skip_pdf_space_and_comments(bytes, data_end);
        if !pdf_keyword_at(bytes, terminator, b"endstream") {
            return Err(invalid_stream());
        }
        (data_end, terminator + b"endstream".len())
    } else {
        let next = find_endstream_bytes(bytes, data_start).ok_or_else(invalid_stream)?;
        (next - b"endstream".len(), next)
    };
    let end = skip_pdf_space_and_comments(bytes, next);
    if !pdf_keyword_at(bytes, end, b"endobj") {
        return Err(invalid_stream());
    }
    Ok((end, Some(data_start..data_end)))
}

fn stream_data<'a>(
    payload: &'a [u8],
    dict: &[u8],
    objects: &HashMap<u32, PdfObject<'_>>,
    budget: &mut PdfDecodeBudget,
) -> Result<&'a [u8]> {
    if budget.deadline.is_some() {
        return Ok(payload);
    }
    if let Some(reference) = probe_dict_ref(dict, b"Length") {
        budget.visit(1)?;
        let scalar = referenced_object(objects, reference).ok_or_else(invalid_stream)?;
        budget.scan(scalar.body.len())?;
        let (length, end) = parse_ascii_usize(&scalar.body, 0).ok_or_else(invalid_stream)?;
        if skip_pdf_space_and_comments(&scalar.body, end) != scalar.body.len() {
            return Err(invalid_stream());
        }
        let remainder = payload.get(length..).ok_or_else(invalid_stream)?;
        budget.scan(remainder.len())?;
        if !remainder.iter().all(u8::is_ascii_whitespace) {
            return Err(invalid_stream());
        }
        return Ok(&payload[..length]);
    }
    if probe_dict_value(dict, b"Length").is_none() {
        // Inline page streams without a declared length are delimited by EOL.
        let mut end = payload.len();
        while end > 0 && matches!(payload[end - 1], b'\r' | b'\n') {
            end -= 1;
        }
        budget.scan(payload.len() - end)?;
        return Ok(&payload[..end]);
    }
    Ok(payload)
}

fn dict_int(dict: &[u8], key: &[u8]) -> Option<usize> {
    let value = probe_dict_value(dict, key)?;
    let (number, end) = parse_ascii_usize(value, 0)?;
    value
        .get(end)
        .is_none_or(|byte| is_pdf_delimiter(*byte))
        .then_some(number)
}

fn parse_ref(bytes: &[u8], index: usize) -> Option<(PdfRef, usize)> {
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
    let generation = std::str::from_utf8(&bytes[gen_start..gen_end])
        .ok()?
        .parse()
        .ok()?;
    let r_at = skip_pdf_space_and_comments(bytes, gen_end);
    if bytes.get(r_at) == Some(&b'R')
        && bytes
            .get(r_at + 1)
            .is_none_or(|byte| is_pdf_delimiter(*byte))
    {
        Some((PdfRef { number, generation }, r_at + 1))
    } else {
        None
    }
}

/// Whether `dict` declares FlateDecode as its (only) stream filter — either
/// `/Filter /FlateDecode` or `/Filter [ /FlateDecode ]`.
fn is_flate_filtered(dict: &[u8]) -> bool {
    let Some(value) = probe_dict_value(dict, b"Filter") else {
        return false;
    };
    if let Some(array) = value.strip_prefix(b"[") {
        let start = skip_pdf_space_and_comments(array, 0);
        return pdf_name_at(array, start, b"FlateDecode")
            && array.get(skip_pdf_space_and_comments(
                array,
                start + b"/FlateDecode".len(),
            )) == Some(&b']');
    }
    pdf_name_at(value, 0, b"FlateDecode")
}

fn has_any_filter(dict: &[u8]) -> bool {
    probe_dict_value(dict, b"Filter").is_some()
}

fn inflate_probe_stream(
    payload: &[u8],
    limit: usize,
    budget: &PdfDecodeBudget,
) -> Result<Option<Vec<u8>>> {
    use miniz_oxide::{
        DataFormat, MZFlush, MZStatus,
        inflate::stream::{InflateState, inflate},
    };

    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    let mut remaining = payload;
    let mut output = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        budget.check_deadline()?;
        let input = &remaining[..remaining.len().min(chunk.len())];
        let step = inflate(&mut state, input, &mut chunk, MZFlush::None);
        remaining = &remaining[step.bytes_consumed..];
        PdfDecodeBudget::reserve_decoded_step(&mut output, &chunk[..step.bytes_written], limit)?;
        if step.status == Ok(MZStatus::StreamEnd) {
            budget.check_deadline()?;
            return Ok(remaining.is_empty().then_some(output));
        }
        if step.status.is_err() || (step.bytes_consumed == 0 && step.bytes_written == 0) {
            return Ok(None);
        }
    }
}

/// Track each chunk before appending to the inflated stream. Passing one
/// extra byte to the inflater distinguishes a full budget from an overflow.
fn inflate_tier_stream(
    payload: &[u8],
    format: miniz_oxide::DataFormat,
    budget: &mut PdfDecodeBudget,
) -> Result<Option<Vec<u8>>> {
    use miniz_oxide::{
        MZFlush, MZStatus,
        inflate::stream::{InflateState, inflate},
    };

    let mut state = InflateState::new_boxed(format);
    let mut remaining = payload;
    let mut output = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        let input = &remaining[..remaining.len().min(chunk.len())];
        let available = budget
            .expansion_remaining()
            .min(budget.memory_limit - budget.retained);
        let limit = chunk.len().min(available.saturating_add(1));
        let step = inflate(&mut state, input, &mut chunk[..limit], MZFlush::None);
        remaining = &remaining[step.bytes_consumed..];
        budget.expand(step.bytes_written)?;
        budget.retain(step.bytes_written)?;
        output.extend_from_slice(&chunk[..step.bytes_written]);
        if step.status == Ok(MZStatus::StreamEnd) {
            return Ok(Some(output));
        }
        if step.status.is_err() || (step.bytes_consumed == 0 && step.bytes_written == 0) {
            budget.release(output.len());
            return Ok(None);
        }
    }
}

/// Effective stream bytes, following only the supported filter chain.
fn effective_stream(
    object: &PdfObject<'_>,
    objects: &HashMap<u32, PdfObject<'_>>,
    budget: &mut PdfDecodeBudget,
) -> Result<Option<Vec<u8>>> {
    budget.check_deadline()?;
    let Some(payload) = object
        .stream
        .clone()
        .and_then(|range| object.body.get(range))
    else {
        return Ok(None);
    };
    let Some(dict) = object.dictionary() else {
        return Ok(None);
    };
    budget.scan(dict.len())?;
    let payload = stream_data(payload, dict, objects, budget)?;
    if probe_dict_type(dict, b"XObject")
        || probe_dict_value(dict, b"Subtype").is_some_and(|value| pdf_name_at(value, 0, b"Image"))
    {
        return Ok(None);
    }
    if is_flate_filtered(dict) {
        if budget.deadline.is_some() {
            let limit = budget.expansion_remaining();
            let data = inflate_probe_stream(payload, limit, budget)?;
            if let Some(ref data) = data {
                budget.expand(data.len())?;
                budget.retain(data.len())?;
            }
            return Ok(data);
        }
        budget.scan(payload.len())?;
        if let Some(data) = inflate_tier_stream(payload, miniz_oxide::DataFormat::Zlib, budget)? {
            return Ok(Some(data));
        }
        inflate_tier_stream(payload, miniz_oxide::DataFormat::Raw, budget)
    } else if has_any_filter(dict) {
        Ok(None)
    } else {
        budget.expand(payload.len())?;
        budget.retain(payload.len())?;
        Ok(Some(payload.to_vec()))
    }
}

fn expand_object_streams<'a>(
    objects: &mut HashMap<u32, PdfObject<'a>>,
    budget: &mut PdfDecodeBudget,
) -> Result<bool> {
    let mut containers = Vec::new();
    for (&number, object) in objects.iter() {
        budget.visit(1)?;
        if object
            .dictionary()
            .is_some_and(|dict| probe_dict_type(dict, b"ObjStm"))
        {
            budget.retain(std::mem::size_of::<u32>())?;
            containers.push(number);
        }
    }
    containers.sort_unstable();
    for &number in &containers {
        let object = &objects[&number];
        let dict = object.dictionary().ok_or_else(invalid_stream)?;
        let count = dict_int(dict, b"N").ok_or_else(invalid_stream)?;
        let first = dict_int(dict, b"First").ok_or_else(invalid_stream)?;
        if has_any_filter(dict) && !is_flate_filtered(dict) {
            // The number and positions of pages in this container are unknown.
            return Ok(false);
        }
        let data = effective_stream(object, objects, budget)?.ok_or_else(invalid_stream)?;
        let offsets = object_stream_offsets(&data, count, first, budget)?;
        for (position, &(objnum, offset)) in offsets.iter().enumerate() {
            if containers.binary_search(&objnum).is_ok() {
                return Err(AdapterError::ContractViolation(
                    "PDF object stream contains a stream container reference cycle or redefinition"
                        .to_owned(),
                ));
            }
            let start = first + offset;
            let end = offsets
                .get(position + 1)
                .map_or(data.len(), |(_, next)| first + next);
            let body = &data[start..end];
            budget.check_memory(body.len())?;
            insert_object(
                objects,
                objnum,
                PdfObject::new(Cow::Owned(body.to_vec()), 0, None),
                budget,
            )?;
        }
        budget.release(data.len() + offsets.len() * std::mem::size_of::<(u32, usize)>());
    }
    budget.release(containers.len() * std::mem::size_of::<u32>());
    Ok(true)
}

fn object_stream_offsets(
    data: &[u8],
    count: usize,
    first: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<Vec<(u32, usize)>> {
    if budget.deadline.is_some() && count > MAX_PROBE_OBJECTS {
        return Err(invalid_stream());
    }
    if first > data.len() || count > first / 3 {
        return Err(AdapterError::ContractViolation(
            "PDF object stream has an impossible object header size".to_owned(),
        ));
    }
    // Header: `objnum offset` pairs (ascii ints) before `first`.
    let header = &data[..first];
    budget.visit(count)?;
    let offsets_bytes = count
        .checked_mul(std::mem::size_of::<(u32, usize)>())
        .ok_or_else(invalid_stream)?;
    budget.retain(offsets_bytes)?;
    let mut numbers_offsets = Vec::with_capacity(count);
    let mut cursor = 0_usize;
    for _ in 0..count {
        budget.check_deadline()?;
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
    let seen_bytes = count
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(invalid_stream)?;
    budget.retain(seen_bytes)?;
    let mut seen_numbers = HashSet::with_capacity(count);
    let mut previous_offset = None;
    for (objnum, offset) in &numbers_offsets {
        budget.check_deadline()?;
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
    budget.release(seen_bytes);
    Ok(numbers_offsets)
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
fn collect_page_numbers(
    objects: &HashMap<u32, PdfObject<'_>>,
    budget: &mut PdfDecodeBudget,
) -> Result<Vec<u32>> {
    let mut pages = Vec::new();
    for (&number, object) in objects {
        budget.visit(1)?;
        if object
            .dictionary()
            .is_some_and(|dict| probe_dict_type(dict, b"Page"))
        {
            budget.retain(std::mem::size_of::<u32>())?;
            pages.push(number);
        }
    }
    pages.sort_unstable();
    validate_page_links(objects, b"Kids", budget)?;
    validate_page_links(objects, b"Parent", budget)?;
    Ok(pages)
}

/// Parent and Kids are separate graphs. Valid trees naturally contain a
/// Parent backlink for each Kids edge.
fn validate_page_links(
    objects: &HashMap<u32, PdfObject<'_>>,
    key: &[u8],
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let retained = budget.retained;
    let mut done = HashSet::new();
    let mut active = HashSet::new();
    let mut stack = Vec::new();
    for (&number, object) in objects {
        let Some(dict) = object.dictionary() else {
            continue;
        };
        if done.contains(&number)
            || !(probe_dict_type(dict, b"Pages") || probe_dict_type(dict, b"Page"))
        {
            continue;
        }
        let Some(links) = probe_dict_value(dict, key) else {
            continue;
        };
        budget
            .retain(std::mem::size_of::<u32>() + std::mem::size_of::<(u32, ReferenceList<'_>)>())?;
        stack.push((number, ReferenceList::new(links)));
        active.insert(number);
        while let Some((number, links)) = stack.last_mut() {
            let Some(reference) = links.next()? else {
                active.remove(number);
                let finished = *number;
                stack.pop();
                budget.release(std::mem::size_of::<(u32, ReferenceList<'_>)>());
                done.insert(finished);
                continue;
            };
            budget.visit(1)?;
            if active.contains(&reference.number) {
                return Err(AdapterError::ContractViolation(
                    "PDF page tree contains a reference cycle".to_owned(),
                ));
            }
            if done.contains(&reference.number) {
                continue;
            }
            let Some(object) = referenced_object(objects, reference) else {
                return Err(AdapterError::ContractViolation(
                    "PDF page tree points to an absent object".to_owned(),
                ));
            };
            let links = if let Some(dict) = object.dictionary() {
                budget.scan(dict.len())?;
                probe_dict_value(dict, key)
            } else {
                None
            };
            let Some(links) = links else {
                budget.retain(std::mem::size_of::<u32>())?;
                done.insert(reference.number);
                continue;
            };
            budget.retain(
                std::mem::size_of::<u32>() + std::mem::size_of::<(u32, ReferenceList<'_>)>(),
            )?;
            stack.push((reference.number, ReferenceList::new(links)));
            active.insert(reference.number);
        }
    }
    budget.retained = retained;
    Ok(())
}

struct ReferenceList<'a> {
    rest: &'a [u8],
    array: bool,
}

impl<'a> ReferenceList<'a> {
    fn new(value: &'a [u8]) -> Self {
        match value.strip_prefix(b"[") {
            Some(rest) => Self { rest, array: true },
            None => Self {
                rest: value,
                array: false,
            },
        }
    }

    fn next(&mut self) -> Result<Option<PdfRef>> {
        let cursor = skip_pdf_space_and_comments(self.rest, 0);
        if self.rest.is_empty()
            || (self.array && self.rest.get(cursor) == Some(&b']'))
            || (!self.array && pdf_keyword_at(self.rest, cursor, b"null"))
        {
            return Ok(None);
        }
        let (reference, after) = parse_ref(self.rest, cursor).ok_or_else(|| {
            AdapterError::ContractViolation("PDF contains an invalid indirect reference".to_owned())
        })?;
        self.rest = if self.array { &self.rest[after..] } else { &[] };
        Ok(Some(reference))
    }
}

fn page_font_maps<'a>(
    objects: &'a HashMap<u32, PdfObject<'_>>,
    page_number: u32,
    budget: &mut PdfDecodeBudget,
) -> Result<HashMap<&'a str, FontMap>> {
    let empty = HashMap::new();
    let Some(page) = objects.get(&page_number).and_then(PdfObject::dictionary) else {
        return Ok(empty);
    };
    budget.visit(1)?;
    budget.scan(page.len())?;
    let Some(resources) = resource_dictionary(objects, page, b"Resources", &[page_number], budget)?
    else {
        return Ok(empty);
    };
    let mut ancestors = vec![page_number];
    ancestors.extend(probe_dict_ref(page, b"Resources").map(|reference| reference.number));
    let Some(font_dict) = resource_dictionary(objects, resources, b"Font", &ancestors, budget)?
    else {
        return Ok(empty);
    };
    ancestors.extend(probe_dict_ref(resources, b"Font").map(|reference| reference.number));
    let mut fonts = HashMap::new();
    budget.scan(font_dict.len())?;
    for (name, reference) in font_entries(font_dict) {
        budget.visit(1)?;
        ensure_acyclic_reference(reference, &ancestors)?;
        if fonts.contains_key(name) {
            return Err(AdapterError::ContractViolation(
                "PDF font dictionary repeats a name".to_owned(),
            ));
        }
        budget.retain(std::mem::size_of::<(&str, FontMap)>())?;
        let map = match font_tounicode_cmap(objects, reference, &ancestors, budget)? {
            Some(cmap) => FontMap::Decoded(cmap),
            None => FontMap::Opaque,
        };
        fonts.insert(name, map);
    }
    Ok(fonts)
}

fn ensure_acyclic_reference(reference: PdfRef, ancestors: &[u32]) -> Result<()> {
    if ancestors.contains(&reference.number) {
        return Err(AdapterError::ContractViolation(
            "PDF contains a reference cycle".to_owned(),
        ));
    }
    Ok(())
}

fn resource_dictionary<'a>(
    objects: &'a HashMap<u32, PdfObject<'_>>,
    dictionary: &'a [u8],
    field: &[u8],
    ancestors: &[u32],
    budget: &mut PdfDecodeBudget,
) -> Result<Option<&'a [u8]>> {
    let Some(value) = probe_dict_value(dictionary, field) else {
        return Ok(None);
    };
    if let Some((reference, _)) = parse_ref(value, 0) {
        budget.visit(1)?;
        ensure_acyclic_reference(reference, ancestors)?;
        let resolved = referenced_object(objects, reference).and_then(PdfObject::dictionary);
        if let Some(dict) = resolved {
            budget.scan(dict.len())?;
        }
        return Ok(resolved);
    }
    Ok(value.starts_with(b"<<").then(|| dict_span(value)).flatten())
}

/// Iterate over borrowed font names so a large dictionary does not need a
/// second collection of owned names and references.
fn font_entries(dict: &[u8]) -> impl Iterator<Item = (&str, PdfRef)> {
    let mut index = 0;
    std::iter::from_fn(move || {
        while index < dict.len() {
            if dict[index] == b'/' {
                let start = index + 1;
                let end = pdf_name_end(dict, start);
                index = skip_pdf_space_and_comments(dict, end);
                if let Some((reference, after)) = parse_ref(dict, index) {
                    index = after;
                    if let Ok(name) = std::str::from_utf8(&dict[start..end]) {
                        return Some((name, reference));
                    }
                }
            } else {
                index += 1;
            }
        }
        None
    })
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
    objects: &HashMap<u32, PdfObject<'_>>,
    font_obj: PdfRef,
    ancestors: &[u32],
    budget: &mut PdfDecodeBudget,
) -> Result<Option<CMap>> {
    let Some(font) = referenced_object(objects, font_obj) else {
        return Ok(None);
    };
    let Some(dict) = font.dictionary() else {
        return Ok(None);
    };
    budget.scan(dict.len())?;
    let Some(reference) = probe_dict_ref(dict, b"ToUnicode") else {
        return Ok(None);
    };
    budget.visit(1)?;
    ensure_acyclic_reference(reference, ancestors)?;
    ensure_acyclic_reference(reference, &[font_obj.number])?;
    let Some(cmap_obj) = referenced_object(objects, reference) else {
        return Ok(None);
    };
    let Some(data) = effective_stream(cmap_obj, objects, budget)? else {
        return Ok(None);
    };
    let cmap = parse_tounicode_cmap(&data, budget)?;
    budget.release(data.len());
    Ok(cmap)
}

// ---------------------------------------------------------------------------
// Step 3: ToUnicode CMap parsing
// ---------------------------------------------------------------------------

fn parse_tounicode_cmap(data: &[u8], budget: &mut PdfDecodeBudget) -> Result<Option<CMap>> {
    let Ok(text) = std::str::from_utf8(data) else {
        return Ok(None);
    };
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
            budget.check_deadline()?;
            let Some((dst, after_dst)) = next_hex_token(after_src) else {
                break;
            };
            if code_bytes == 0 {
                code_bytes = (src.len() / 2).max(1);
            }
            if let Some(code) = hex_to_code(src) {
                insert_cmap_hex(&mut map, code, dst, budget)?;
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
            budget.check_deadline()?;
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
                    budget.check_deadline()?;
                    let Some((dst, after_dst)) = next_hex_token(inner) else {
                        break;
                    };
                    insert_cmap_hex(&mut map, code, dst, budget)?;
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
                let Some(mut units) = hex_to_utf16_units(dst_hex, budget)? else {
                    break;
                };
                let span = high - low;
                for step in 0..=span {
                    let code = low + step;
                    insert_cmap_target(&mut map, code, &units, budget)?;
                    if let Some(last) = units.last_mut() {
                        *last = last.wrapping_add(1);
                    }
                }
                budget.release(units.len() * std::mem::size_of::<u16>());
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
    let mut value = 0u32;
    let mut count = 0;
    for digit in hex.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        value = value.checked_mul(16)? + char::from(digit).to_digit(16)?;
        count += 1;
        if count > 8 {
            return None;
        }
    }
    (count != 0).then_some(value)
}

fn hex_to_utf16_units(hex: &str, budget: &mut PdfDecodeBudget) -> Result<Option<Vec<u16>>> {
    let digits = hex.bytes().filter(|byte| !byte.is_ascii_whitespace());
    let count = digits.clone().count();
    let width = if count == 2 { 2 } else { 4 };
    if count == 0 || !count.is_multiple_of(width) {
        return Ok(None);
    }
    let bytes = (count / width) * std::mem::size_of::<u16>();
    budget.retain(bytes)?;
    let mut units = Vec::with_capacity(count / width);
    let mut unit = 0;
    for (offset, digit) in digits.enumerate() {
        let Some(digit) = char::from(digit).to_digit(16) else {
            budget.release(bytes);
            return Ok(None);
        };
        unit = (unit << 4) | digit as u16;
        if (offset + 1) % width == 0 {
            units.push(unit);
            unit = 0;
        }
    }
    Ok(Some(units))
}

fn insert_cmap_hex(
    map: &mut HashMap<u32, String>,
    code: u32,
    dst: &str,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    if let Some(units) = hex_to_utf16_units(dst, budget)? {
        insert_cmap_target(map, code, &units, budget)?;
        budget.release(units.len() * std::mem::size_of::<u16>());
    }
    Ok(())
}

fn insert_cmap_target(
    map: &mut HashMap<u32, String>,
    code: u32,
    units: &[u16],
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    budget.visit(1)?;
    let chars = || {
        char::decode_utf16(units.iter().copied()).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
    };
    let len = chars().map(char::len_utf8).sum::<usize>();
    budget.expand(len)?;
    let entry_bytes = if map.contains_key(&code) {
        0
    } else {
        std::mem::size_of::<(u32, String)>()
    };
    budget.retain(len + entry_bytes)?;
    let mut target = String::with_capacity(len);
    target.extend(chars());
    if let Some(previous) = map.insert(code, target) {
        budget.release(previous.len());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 4: content-stream decoding
// ---------------------------------------------------------------------------

fn decode_page(
    objects: &HashMap<u32, PdfObject<'_>>,
    page_number: u32,
    fonts: &HashMap<&str, FontMap>,
    budget: &mut PdfDecodeBudget,
) -> Result<String> {
    let Some(page) = objects.get(&page_number) else {
        return Ok(String::new());
    };
    let Some(dict) = page.dictionary() else {
        return Ok(String::new());
    };
    if budget.deadline.is_some() {
        let mut content = Vec::new();
        for reference in xref::content_references(dict, budget)? {
            ensure_acyclic_reference(reference, &[page_number])?;
            let object = referenced_object(objects, reference).ok_or_else(|| {
                AdapterError::ContractViolation(
                    "probe PDF content reference is absent from the active xref".to_owned(),
                )
            })?;
            append_stream(objects, object, &mut content, budget)?;
        }
        return decode_content_ops(&content, fonts, budget);
    }
    let Some(value) = probe_dict_value(dict, b"Contents") else {
        // The supported literal-page subset has the stream on the page itself.
        let Some(content) = effective_stream(page, objects, budget)? else {
            return Ok(String::new());
        };
        return decode_content_ops(&content, fonts, budget);
    };
    let content = collect_page_content(objects, page_number, value, budget)?;
    decode_content_ops(&content, fonts, budget)
}

fn collect_page_content(
    objects: &HashMap<u32, PdfObject<'_>>,
    page_number: u32,
    value: &[u8],
    budget: &mut PdfDecodeBudget,
) -> Result<Vec<u8>> {
    let frame_bytes = std::mem::size_of::<(u32, ReferenceList<'_>)>() + std::mem::size_of::<u32>();
    budget.retain(frame_bytes)?;
    let mut stack = vec![(page_number, ReferenceList::new(value))];
    let mut active = HashSet::from([page_number]);
    let mut content = Vec::new();
    while let Some((container, refs)) = stack.last_mut() {
        let Some(reference) = refs.next()? else {
            active.remove(container);
            stack.pop();
            budget.release(frame_bytes);
            continue;
        };
        budget.visit(1)?;
        if active.contains(&reference.number) {
            return Err(AdapterError::ContractViolation(
                "PDF content contains a reference cycle".to_owned(),
            ));
        }
        let object = content_object(objects, reference)?;
        if object.stream.is_some() {
            append_stream(objects, object, &mut content, budget)?;
        } else {
            budget.scan(object.body.len())?;
            let start = skip_pdf_space_and_comments(&object.body, 0);
            let value = &object.body[start..];
            if value.starts_with(b"[") || parse_ref(value, 0).is_some() {
                budget.retain(frame_bytes)?;
                stack.push((reference.number, ReferenceList::new(value)));
                active.insert(reference.number);
            }
        }
    }
    Ok(content)
}

fn content_object<'a, 'pdf>(
    objects: &'a HashMap<u32, PdfObject<'pdf>>,
    reference: PdfRef,
) -> Result<&'a PdfObject<'pdf>> {
    referenced_object(objects, reference).ok_or_else(|| {
        AdapterError::ContractViolation(
            "PDF content reference points to an absent object".to_owned(),
        )
    })
}

fn append_stream(
    objects: &HashMap<u32, PdfObject<'_>>,
    object: &PdfObject<'_>,
    content: &mut Vec<u8>,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    if let Some(data) = effective_stream(object, objects, budget)? {
        if content.is_empty() {
            *content = data;
        } else {
            budget.retain(data.len() + 1)?;
            content.push(b'\n');
            content.extend_from_slice(&data);
            budget.release(data.len());
        }
    } else if budget.deadline.is_some() {
        return Err(AdapterError::ContractViolation(
            "probe PDF content reference is not a decodable stream".to_owned(),
        ));
    }
    Ok(())
}

// A single TJ operand list normally has tens to thousands of items. Permit
// 262,144 operands and 16 MiB of representation (including enum slots), but
// charge cumulative admissions per content stream so nested/restarted or
// unterminated arrays cannot reset the work bound.
const MAX_CONTENT_OPERANDS: usize = 262_144;
const MAX_CONTENT_OPERAND_BYTES: usize = 16 * 1024 * 1024;

enum ShowString<'a> {
    Literal(&'a [u8]),
    Hex(&'a [u8]),
}

enum ArrayItem<'a> {
    Text(ShowString<'a>),
    Kern(f64),
}

/// Tokenize a content stream, tracking the current font through `Tf`, and
/// decode every show operator (`Tj`, `TJ`, `'`, `"`).
fn decode_content_ops(
    content: &[u8],
    fonts: &HashMap<&str, FontMap>,
    budget: &mut PdfDecodeBudget,
) -> Result<String> {
    decode_content_ops_with_limits(
        content,
        fonts,
        budget,
        budget
            .deadline
            .is_some()
            .then_some((MAX_CONTENT_OPERANDS, MAX_CONTENT_OPERAND_BYTES)),
    )
}

const CONTENT_SCAN_INTERVAL: usize = 4096;

fn check_content_scan(budget: &PdfDecodeBudget, next_check: &mut usize, at: usize) -> Result<()> {
    if at >= *next_check {
        budget.check_deadline()?;
        *next_check = at + CONTENT_SCAN_INTERVAL;
    }
    Ok(())
}

/// Scan only as far as the next deadline checkpoint, even within a single token.
fn scan_content_until(
    content: &[u8],
    mut at: usize,
    budget: &PdfDecodeBudget,
    next_check: &mut usize,
    stop: impl Fn(u8) -> bool,
) -> Result<usize> {
    while at < content.len() {
        check_content_scan(budget, next_check, at)?;
        let end = (*next_check).min(content.len());
        if let Some(offset) = content[at..end].iter().copied().position(&stop) {
            return Ok(at + offset);
        }
        at = end;
    }
    Ok(at)
}

fn decode_content_ops_with_limits(
    content: &[u8],
    fonts: &HashMap<&str, FontMap>,
    budget: &mut PdfDecodeBudget,
    probe_limits: Option<(usize, usize)>,
) -> Result<String> {
    let mut operands = 0;
    let mut operand_bytes = 0;
    let mut admit_operand = |payload: usize| -> Result<()> {
        let Some((operand_limit, byte_limit)) = probe_limits else {
            return Ok(());
        };
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
    let mut last_name = None;
    let mut last_string = None;
    let mut array_items: Option<Vec<ArrayItem<'_>>> = None;
    let item_bytes = std::mem::size_of::<ArrayItem<'_>>();

    let mut next_deadline_check = 0;
    while index < content.len() {
        check_content_scan(budget, &mut next_deadline_check, index)?;
        let byte = content[index];
        match byte {
            b'%' => {
                let end =
                    scan_content_until(content, index, budget, &mut next_deadline_check, |b| {
                        matches!(b, b'\r' | b'\n')
                    })?;
                index = (end + 1).min(content.len());
            }
            b'(' => {
                let end = skip_pdf_literal_string_checked(content, index, |at| {
                    check_content_scan(budget, &mut next_deadline_check, at)
                })?;
                admit_operand(end.saturating_sub(index + 2))?;
                let start = index + 1;
                let closed = end > start && content[end - 1] == b')';
                let string = ShowString::Literal(&content[start..end - usize::from(closed)]);
                match array_items.as_mut() {
                    Some(items) => {
                        budget.retain(item_bytes)?;
                        items.push(ArrayItem::Text(string));
                    }
                    None => last_string = Some(string),
                }
                index = end;
            }
            b'<' if content.get(index + 1) == Some(&b'<') => {
                // inline dictionary (BDC property lists): skip balanced
                let mut depth = 0_usize;
                while index < content.len() {
                    check_content_scan(budget, &mut next_deadline_check, index)?;
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
                let end = scan_content_until(
                    content,
                    index + 1,
                    budget,
                    &mut next_deadline_check,
                    |b| b == b'>',
                )?;
                // Encoded length is a conservative bound for decoded storage.
                admit_operand(end - index - 1)?;
                let string = ShowString::Hex(&content[index + 1..end]);
                match array_items.as_mut() {
                    Some(items) => {
                        budget.retain(item_bytes)?;
                        items.push(ArrayItem::Text(string));
                    }
                    None => last_string = Some(string),
                }
                index = end + 1;
            }
            b'[' => {
                admit_operand(0)?;
                if let Some(items) = array_items.replace(Vec::new()) {
                    budget.release(items.len() * item_bytes);
                }
                index += 1;
            }
            b']' => index += 1,
            b'/' => {
                let end = scan_content_until(
                    content,
                    index + 1,
                    budget,
                    &mut next_deadline_check,
                    is_pdf_delimiter,
                )?;
                admit_operand(end - index - 1)?;
                last_name = std::str::from_utf8(&content[index + 1..end]).ok();
                index = end;
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => {
                let end = scan_content_until(
                    content,
                    index + 1,
                    budget,
                    &mut next_deadline_check,
                    |b| !(b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+')),
                )?;
                if let Some(items) = array_items.as_mut()
                    && let Ok(value) = std::str::from_utf8(&content[index..end])
                        .unwrap_or("")
                        .parse::<f64>()
                {
                    admit_operand(0)?;
                    budget.retain(item_bytes)?;
                    items.push(ArrayItem::Kern(value));
                }
                index = end;
            }
            _ if byte.is_ascii_alphabetic() || byte == b'\'' || byte == b'"' => {
                let end = if byte == b'\'' || byte == b'"' {
                    index + 1
                } else {
                    scan_content_until(content, index + 1, budget, &mut next_deadline_check, |b| {
                        !(b.is_ascii_alphabetic() || matches!(b, b'*' | b'0' | b'1'))
                    })?
                };
                let op = &content[index..end];
                match op {
                    b"Tf" => {
                        current_font = last_name.and_then(|name| fonts.get(name));
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
                            let retained = items.len() * item_bytes;
                            for item in items {
                                budget.check_deadline()?;
                                match item {
                                    ArrayItem::Text(string) => {
                                        if let Some(text) =
                                            decode_show_string(&string, current_font, budget)?
                                        {
                                            append_working_text(&mut assembled, &text, budget)?;
                                            budget.release(text.len());
                                        }
                                    }
                                    ArrayItem::Kern(value) => {
                                        if value.abs() >= TJ_WORD_GAP_THRESHOLD
                                            && !assembled.ends_with(' ')
                                        {
                                            append_working_text(&mut assembled, " ", budget)?;
                                        }
                                    }
                                }
                            }
                            budget.release(retained);
                            push_decoded(&mut output, Some(assembled), budget)?;
                        }
                    }
                    _ => {}
                }
                index = end;
            }
            _ => index += 1,
        }
    }
    Ok(output)
}

fn append_working_text(out: &mut String, text: &str, budget: &mut PdfDecodeBudget) -> Result<()> {
    budget.retain(text.len())?;
    out.push_str(text);
    Ok(())
}

fn append_bounded_text(out: &mut String, text: &str, budget: &mut PdfDecodeBudget) -> Result<()> {
    let next = out.len() + text.len();
    budget.check_text(next)?;
    append_working_text(out, text, budget)
}

fn push_decoded(
    output: &mut String,
    text: Option<String>,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let Some(text) = text else { return Ok(()) };
    let trimmed = text.trim();
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
    budget.release(text.len());
    Ok(())
}

fn decode_show_string(
    string: &ShowString<'_>,
    font: Option<&FontMap>,
    budget: &mut PdfDecodeBudget,
) -> Result<Option<String>> {
    if matches!(font, Some(FontMap::Opaque))
        || (font.is_none() && matches!(string, ShowString::Hex(_)))
    {
        return Ok(None);
    }
    let (bytes, retained) = match string {
        ShowString::Literal(raw) => {
            budget.retain(raw.len())?;
            (unescape_pdf_literal(raw), raw.len())
        }
        ShowString::Hex(raw) => {
            let len = raw
                .iter()
                .filter(|byte| byte.is_ascii_hexdigit())
                .count()
                .div_ceil(2);
            budget.retain(len)?;
            (hex_bytes(raw, len), len)
        }
    };
    let out = match font {
        Some(FontMap::Decoded(cmap)) => {
            let mut out = String::new();
            for (index, chunk) in bytes.chunks(cmap.code_bytes).enumerate() {
                if index % 1024 == 0 {
                    budget.check_deadline()?;
                }
                let mut code = 0_u32;
                for byte in chunk {
                    code = (code << 8) | u32::from(*byte);
                }
                if let Some(mapped) = cmap.map.get(&code) {
                    budget.expand(mapped.len())?;
                    append_working_text(&mut out, mapped, budget)?;
                }
            }
            out
        }
        Some(FontMap::Opaque) => unreachable!("opaque font rejected above"),
        None => {
            let text = lossy_text(&bytes, budget, false)?;
            budget.retain(text.len())?;
            text
        }
    };
    budget.release(retained);
    Ok(Some(out))
}

fn lossy_text(bytes: &[u8], budget: &PdfDecodeBudget, unit: bool) -> Result<String> {
    let len = bytes
        .utf8_chunks()
        .map(|chunk| {
            chunk.valid().len()
                + if chunk.invalid().is_empty() {
                    0
                } else {
                    char::REPLACEMENT_CHARACTER.len_utf8()
                }
        })
        .sum();
    if unit {
        budget.check_text(len)?;
    }
    budget.check_memory(len)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn hex_bytes(raw: &[u8], len: usize) -> Vec<u8> {
    let mut digits = raw.iter().filter_map(|byte| char::from(*byte).to_digit(16));
    let mut out = Vec::with_capacity(len);
    while let Some(high) = digits.next() {
        out.push(((high << 4) | digits.next().unwrap_or(0)) as u8);
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

    fn read_pages(bytes: &[u8]) -> Result<Vec<String>> {
        let mut pdf = PdfPages::open(bytes, ResourceTier::Standard)?;
        (0..pdf.len()).map(|index| pdf.read_page(index)).collect()
    }

    fn assert_limit(error: AdapterError, resource: Resource, limit: u64, observed: u64) {
        let AdapterError::ResourceLimit(error) = error else {
            panic!("expected resource exhaustion, got {error}");
        };
        assert_eq!(error.resource, resource);
        assert_eq!(error.limit, Some(limit));
        assert_eq!(error.observed, Some(observed));
    }

    #[test]
    fn long_content_tokens_check_the_deadline_before_reaching_their_end() {
        for (start, payload, end) in [
            ("%", 'x', ""),
            ("<< /Key ", 'x', " >>"),
            ("(", 'x', ")"),
            ("<", 'a', ">"),
            ("/", 'x', ""),
            ("", '1', ""),
            ("", 'X', ""),
        ] {
            let content = format!("{start}{}{end}", payload.to_string().repeat(32 * 1024));
            let mut budget = PdfDecodeBudget {
                checks_before_timeout: Some(std::cell::Cell::new(1)),
                ..PdfDecodeBudget::default()
            };
            let error = decode_content_ops(content.as_bytes(), &HashMap::new(), &mut budget)
                .expect_err(start);
            assert!(error.to_string().contains("time limit"), "{error}");
        }
    }

    #[test]
    fn one_byte_bfchar_flate_content_decodes() {
        let pdf = one_byte_cid_pdf();
        let pages = read_pages(&pdf).expect("decode");
        // <01><02> = "Sy", <03> = "nt" (multi-unit target), kern -420 = word
        // gap, <01><04> = "S!".
        assert_eq!(pages, vec!["Synt S!".to_owned()]);
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

        let pages = read_pages(&pdf).expect("decode");
        // <0024><0025> map to A,B; kern -375 = word gap; <0026> = C.
        assert_eq!(pages, vec!["AB C".to_owned()]);
    }

    /// 2026-07-23 fleet-OCR regression: TeX Live packs every /Type /Page
    /// dictionary (often beyond the linearized first page) inside a
    /// compressed /Type /ObjStm, so the RAW structural scan undercounts and
    /// The separate raw scanner used to truncate the graph decoder's
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

        let pages = read_pages(&pdf).expect("decode");
        assert_eq!(pages.len(), 3, "{pages:?}");
        // End to end through the extract entry (raw structural count here is
        // ZERO — the old normalization would have truncated to 1).
        assert!(pages[2].contains("objstm page 3 body"), "{pages:?}");
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
        assert!(
            read_pages(&pdf)
                .expect("decode")
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn flate_expansion_checks_memory_at_exact_boundary() {
        let mut budget = test_budget();
        budget.memory_limit = 4096;
        let data = zlib(&[b'x'; 4096]);
        let expanded = inflate_tier_stream(&data, miniz_oxide::DataFormat::Zlib, &mut budget)
            .unwrap()
            .unwrap();
        assert_eq!(expanded.len(), 4096);
        budget.release(4096);
        let data = zlib(&[b'x'; 4097]);
        let error =
            inflate_tier_stream(&data, miniz_oxide::DataFormat::Zlib, &mut budget).unwrap_err();
        assert_limit(error, Resource::Memory, 4096, 4097);
    }

    #[test]
    fn expansion_and_visits_share_one_work_budget_across_chunks() {
        let mut budget = test_budget();
        budget.work_limit = 3;
        budget.visit(1).unwrap();
        for _ in 0..2048 {
            budget.expand(1).unwrap();
        }
        assert_eq!(budget.work, 3);
        assert_limit(budget.expand(1).unwrap_err(), Resource::Work, 3, 4);
    }

    #[test]
    fn flate_expansion_over_work_limit_stops_before_retaining_excess() {
        let mut budget = test_budget();
        budget.work_limit = 4;
        let data = zlib(&[b'x'; 4097]);
        let error =
            inflate_tier_stream(&data, miniz_oxide::DataFormat::Zlib, &mut budget).unwrap_err();
        assert_limit(error, Resource::Work, 4, 5);
        assert!(budget.retained <= 4096);
    }

    #[test]
    fn corrupt_flate_stream_falls_back_to_none() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(
            2,
            "<< /Length 11 /Filter /FlateDecode >>",
            b"\x01\x02\x03\x04garbage",
        ));
        pdf.extend_from_slice(b"%%EOF\n");
        assert!(
            read_pages(&pdf)
                .expect("decode")
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn uncompressed_literal_content_still_decodes_via_graph() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(
            2,
            "<< /Length 29 >>",
            b"BT (plain fixture text) Tj ET",
        ));
        pdf.extend_from_slice(b"%%EOF\n");
        let pages = read_pages(&pdf).expect("decode");
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
        assert!(
            read_pages(&pdf)
                .expect("decode")
                .iter()
                .all(String::is_empty)
        );
    }

    #[test]
    fn literal_escapes_and_octal_unescape() {
        assert_eq!(
            unescape_pdf_literal(b"a\\(b\\)c\\\\d\\n\\101"),
            b"a(b)c\\d\nA".to_vec()
        );
    }

    #[test]
    fn compact_numeric_array_hits_probe_operand_limit() {
        // About 512 KiB of input used to queue all operands without a semantic
        // bound. Keep this regression far below the 16 MiB stream ceiling.
        let content = format!("[{}", "0 ".repeat(MAX_CONTENT_OPERANDS));
        assert!(matches!(
            decode_content_ops_with_limits(
                content.as_bytes(),
                &HashMap::new(),
                &mut test_budget(),
                Some((MAX_CONTENT_OPERANDS, MAX_CONTENT_OPERAND_BYTES))
            ),
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
                decode_content_ops_with_limits(
                    content,
                    &fonts,
                    &mut test_budget(),
                    Some((3, 1024))
                )
                .is_ok()
            );
            assert!(matches!(
                decode_content_ops_with_limits(
                    content,
                    &fonts,
                    &mut test_budget(),
                    Some((2, 1024))
                ),
                Err(AdapterError::ContractViolation(_))
            ));
        }
    }

    #[test]
    fn content_operand_representation_exact_boundary_preserves_text() {
        let content = b"[(hello) -200 (world)] TJ";
        let bytes = 4 * std::mem::size_of::<ArrayItem>() + 10;
        assert_eq!(
            decode_content_ops_with_limits(
                content,
                &HashMap::new(),
                &mut test_budget(),
                Some((4, bytes))
            )
            .unwrap(),
            "hello world"
        );
        assert!(matches!(
            decode_content_ops_with_limits(
                content,
                &HashMap::new(),
                &mut test_budget(),
                Some((4, bytes - 1))
            ),
            Err(AdapterError::ContractViolation(_))
        ));
    }

    fn test_budget() -> PdfDecodeBudget {
        PdfDecodeBudget::default()
    }

    #[test]
    fn short_bfrange_cannot_amplify_past_shared_work_budget() {
        let mut budget = test_budget();
        budget.work_limit = 129;
        let cmap = b"1 beginbfrange\n<0000> <00FF> <0041>\nendbfrange";
        let error = parse_tounicode_cmap(cmap, &mut budget).unwrap_err();
        assert_limit(error, Resource::Work, 129, 130);
    }

    #[test]
    fn lossy_cmap_target_uses_actual_utf8_size() {
        let mut budget = test_budget();
        let entry = std::mem::size_of::<(u32, String)>();
        budget.memory_limit = entry + 2 + 3;
        let cmap = b"1 beginbfchar\n<01> <D800>\nendbfchar";
        let output = parse_tounicode_cmap(cmap, &mut budget).unwrap().unwrap();
        assert_eq!(output.map[&1], "\u{fffd}");
        assert_eq!(budget.retained, entry + 3);
        let mut budget = test_budget();
        budget.memory_limit = entry + 2 + 2;
        let error = parse_tounicode_cmap(cmap, &mut budget).unwrap_err();
        assert_limit(
            error,
            Resource::Memory,
            (entry + 4) as u64,
            (entry + 5) as u64,
        );
    }

    #[test]
    fn incremental_object_redefinition_keeps_last_definition() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        pdf.extend(stream_obj(2, "<< /Length 22 >>", b"BT (old content) Tj ET"));
        pdf.extend(obj(1, b"<< /Type /Page /Contents 3 0 R >>"));
        pdf.extend(stream_obj(3, "<< /Length 22 >>", b"BT (new content) Tj ET"));
        let pages = read_pages(&pdf).expect("decode");
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
        let error = read_pages(&pdf).expect_err("hostile /N must be rejected");
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
            let error = read_pages(&pdf).expect_err("non-monotonic offsets must fail");
            assert!(matches!(error, AdapterError::ContractViolation(_)));
        }
    }

    #[test]
    fn object_stream_accepts_more_than_8192_members() {
        let mut header = String::new();
        let mut bodies = Vec::new();
        for id in 3..=8195 {
            use std::fmt::Write;
            write!(header, "{id} {} ", bodies.len()).unwrap();
            let body: &[u8] = if id == 8195 {
                b"<< /Type /Page /Contents 2 0 R >>"
            } else {
                b"null "
            };
            bodies.extend_from_slice(body);
        }
        let first = header.len();
        let mut data = header.into_bytes();
        data.extend_from_slice(&bodies);
        let packed = zlib(&data);
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(stream_obj(
            1,
            &format!(
                "<< /Type /ObjStm /N 8193 /First {first} /Length {} /Filter /FlateDecode >>",
                packed.len()
            ),
            &packed,
        ));
        pdf.extend(stream_obj(2, "<< /Length 14 >>", b"BT (packed) Tj"));
        assert_eq!(read_pages(&pdf).unwrap(), ["packed"]);
    }

    #[test]
    fn indirect_contents_array_decodes_and_rejects_cycles() {
        let mut base = b"%PDF-1.6\n".to_vec();
        base.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        base.extend(obj(2, b"[3 0 R]"));
        let mut cyclic = base.clone();
        cyclic.extend(obj(3, b"[2 0 R]"));
        let error = read_pages(&cyclic).unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
        assert!(error.to_string().contains("cycle"));

        base.extend(stream_obj(3, "<< /Length 17 >>", b"BT (indirect) Tj\n"));
        assert_eq!(read_pages(&base).unwrap(), ["indirect"]);
    }

    #[test]
    fn stream_length_uses_direct_offset_and_checks_indirect_length() {
        let payload = b"BT (literal\nendstream\nremains visible) Tj ET";
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents 2 0 R >>"));
        let mut direct = pdf.clone();
        direct.extend(stream_obj(
            2,
            &format!("<< /Length {} >>", payload.len()),
            payload,
        ));
        assert_eq!(
            read_pages(&direct).unwrap(),
            ["literal\nendstream\nremains visible"]
        );

        let payload = b"BT (indirect) Tj";
        pdf.extend(stream_obj(2, "<< /Length 3 0 R >>", payload));
        let mut invalid = pdf.clone();
        invalid.extend(obj(3, b"4"));
        assert!(matches!(
            read_pages(&invalid),
            Err(AdapterError::ContractViolation(_))
        ));
        pdf.extend(obj(3, payload.len().to_string().as_bytes()));
        assert_eq!(read_pages(&pdf).unwrap(), ["indirect"]);
    }

    #[test]
    fn repeated_content_references_charge_work_and_release_each_page() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        pdf.extend(obj(1, b"<< /Type /Page /Contents [3 0 R 3 0 R] >>"));
        pdf.extend(obj(2, b"<< /Type /Page /Contents [3 0 R 3 0 R] >>"));
        let payload = [b' '; 1024];
        let dictionary = format!("<< /Length 1024 /Note ({}) >>", "x".repeat(2048));
        pdf.extend(stream_obj(3, &dictionary, &payload));

        let mut pages = PdfPages::open(&pdf, ResourceTier::Standard).unwrap();
        let baseline = pages.budget.retained;
        let traversal =
            std::mem::size_of::<(u32, ReferenceList<'_>)>() + std::mem::size_of::<u32>();
        pages.budget.memory_limit = baseline + traversal + 3 * payload.len() + 1;
        let dictionary_work = dictionary.len().div_ceil(1024) as u64;
        pages.budget.work_limit = pages.budget.work + 12 + 4 * dictionary_work;
        for index in 0..2 {
            assert!(pages.read_page(index).unwrap().is_empty());
            assert_eq!(pages.budget.retained, baseline);
        }
        assert_eq!(pages.budget.work, pages.budget.work_limit);
        let limit = pages.budget.work_limit;
        assert_limit(
            pages.read_page(0).unwrap_err(),
            Resource::Work,
            limit,
            limit + 1,
        );
    }
}
