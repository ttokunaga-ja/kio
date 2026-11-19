//! Resolve active cross-reference entries in PDF revisions.
use super::{
    AdapterError, HashMap, MAX_PROBE_OBJECTS, Objects, PackedObject, PdfDecodeBudget, PdfObject,
    PdfRef, Result, dict_span, effective_stream, find_endobj, is_pdf_delimiter,
    load_packed_container, member_range, parse_ref, pdf_keyword_at, probe_dict_ref,
    probe_dict_type, probe_dict_value, skip_pdf_space_and_comments,
};
use std::borrow::Cow;
use std::ops::Range;

const MAX_XREF_SECTIONS: usize = 16;
const MAX_XREF_FIELD_BYTES: usize = 8;

#[derive(Clone, Copy)]
enum Entry {
    Free,
    Direct { offset: usize, generation: u16 },
    Packed { stream: u32, member: usize },
}

struct Section<'a> {
    entries: HashMap<u32, Entry>,
    trailer: &'a [u8],
}

pub(super) fn probe_objects<'a>(
    pdf: &'a [u8],
    budget: &mut PdfDecodeBudget,
) -> Result<(PdfRef, HashMap<u32, PdfObject<'a>>)> {
    let (root, objects) = load_index(pdf, budget)?;
    Ok((root, objects.direct))
}

pub(super) fn declares_xref(pdf: &[u8]) -> bool {
    startxref(pdf).is_some()
}

pub(super) fn load_index<'a>(
    pdf: &'a [u8],
    budget: &mut PdfDecodeBudget,
) -> Result<(PdfRef, Objects<'a>)> {
    budget.scan(pdf.len())?;
    let (offset, footer) = startxref(pdf).ok_or_else(invalid)?;
    let pdf = &pdf[..footer];
    let mut offset = Some(offset);
    let mut root = None;
    let mut entries = HashMap::new();
    let mut section_offsets = Vec::new();
    let mut row_count = 0;
    while let Some(position) = offset {
        budget.visit(1)?;
        if budget.deadline.is_some() && section_offsets.len() == MAX_XREF_SECTIONS {
            return Err(invalid());
        }
        let mut section = if pdf_keyword_at(pdf, position, b"xref") {
            classic_section(pdf, position, budget)?
        } else {
            stream_section(pdf, position, budget)?
        };
        budget.reserve_vec(&mut section_offsets, 1)?;
        section_offsets.push(position);
        row_count += section.entries.len();
        if let Some(supplement) = optional_uint(section.trailer, b"XRefStm")? {
            if !pdf_keyword_at(pdf, position, b"xref")
                || supplement >= position
                || (budget.deadline.is_some() && section_offsets.len() == MAX_XREF_SECTIONS)
            {
                return Err(invalid());
            }
            let supplemental = stream_section(pdf, supplement, budget)?;
            budget.reserve_vec(&mut section_offsets, 1)?;
            section_offsets.push(supplement);
            row_count += supplemental.entries.len();
            // In a hybrid revision the stream's entries take precedence.
            for (number, entry) in supplemental.entries {
                if section.entries.insert(number, entry).is_some() {
                    budget.release(std::mem::size_of::<(u32, Entry)>());
                }
            }
        }
        if probe_dict_value(section.trailer, b"Encrypt").is_some() {
            return Err(invalid());
        }
        if budget.deadline.is_some() && row_count > MAX_PROBE_OBJECTS {
            return Err(invalid());
        }
        if root.is_none() && probe_dict_value(section.trailer, b"Root").is_some() {
            root = Some(probe_dict_ref(section.trailer, b"Root").ok_or_else(invalid)?);
        }
        for (number, entry) in section.entries {
            budget.visit(1)?;
            if let std::collections::hash_map::Entry::Vacant(slot) = entries.entry(number) {
                slot.insert(entry);
            } else {
                budget.release(std::mem::size_of::<(u32, Entry)>());
            }
        }
        offset = optional_uint(section.trailer, b"Prev")?;
        if offset.is_some_and(|previous| previous >= position) {
            return Err(invalid());
        }
    }
    section_offsets.sort_unstable();
    let objects = load_objects(pdf, &entries, &section_offsets, budget)?;
    budget.release(
        section_offsets.capacity() * std::mem::size_of::<usize>()
            + entries.len() * std::mem::size_of::<(u32, Entry)>(),
    );
    Ok((root.ok_or_else(invalid)?, objects))
}

fn startxref(pdf: &[u8]) -> Option<(usize, usize)> {
    if !pdf.starts_with(b"%PDF-") {
        return None;
    }
    let end = pdf
        .trim_ascii_end()
        .strip_suffix(b"%%EOF")?
        .trim_ascii_end();
    let number = end.iter().rposition(|byte| !byte.is_ascii_digit())? + 1;
    let offset = decimal(&end[number..])?;
    let keyword = end[..number].trim_ascii_end();
    let footer = keyword.len().checked_sub(b"startxref".len())?;
    if !pdf_keyword_at(keyword, footer, b"startxref") {
        return None;
    }
    (offset < footer).then_some((offset, footer))
}

fn classic_section<'a>(
    pdf: &'a [u8],
    offset: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<Section<'a>> {
    let mut cursor = skip_pdf_space_and_comments(pdf, offset + 4);
    let mut entries = HashMap::new();
    while !pdf_keyword_at(pdf, cursor, b"trailer") {
        budget.check_deadline()?;
        let (first, next) = unsigned(pdf, cursor).ok_or_else(invalid)?;
        let (count, next) = unsigned(pdf, next).ok_or_else(invalid)?;
        let range = row_range(first, count, entries.len(), budget)?;
        cursor = skip_pdf_space_and_comments(pdf, next);
        for number in range {
            budget.check_deadline()?;
            let row = pdf.get(cursor..).ok_or_else(invalid)?;
            let length = row
                .iter()
                .take(22)
                .position(|byte| matches!(byte, b'\r' | b'\n'))
                .ok_or_else(invalid)?;
            insert(&mut entries, number, classic_entry(&row[..length])?, budget)?;
            cursor = skip_pdf_space_and_comments(pdf, cursor + length);
        }
    }
    let trailer = direct_dict(pdf, cursor + 7).ok_or_else(invalid)?;
    verify_size(trailer, &entries, budget)?;
    Ok(Section { entries, trailer })
}

fn classic_entry(row: &[u8]) -> Result<Entry> {
    let mut fields = row
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let offset = fields
        .next()
        .filter(|field| field.len() == 10)
        .and_then(decimal)
        .ok_or_else(invalid)?;
    let generation = fields
        .next()
        .filter(|field| field.len() == 5)
        .and_then(decimal)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(invalid)?;
    let entry = match fields.next() {
        Some(b"n") => Entry::Direct { offset, generation },
        Some(b"f") => Entry::Free,
        _ => return Err(invalid()),
    };
    if fields.next().is_some() {
        return Err(invalid());
    }
    Ok(entry)
}

fn stream_section<'a>(
    pdf: &'a [u8],
    offset: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<Section<'a>> {
    let (reference, start) = header(pdf, offset).ok_or_else(invalid)?;
    let trailer = direct_dict(pdf, start).ok_or_else(invalid)?;
    if !probe_dict_type(trailer, b"XRef") {
        return Err(invalid());
    }
    let definition = object_at(
        pdf,
        start..pdf.len(),
        reference.generation,
        &HashMap::new(),
        budget,
    )?;
    let mut data = effective_stream(&definition, &HashMap::new(), budget)?.ok_or_else(invalid)?;
    let widths = probe_dict_value(trailer, b"W")
        .and_then(|value| uint_array(value, 3))
        .ok_or_else(invalid)?;
    if widths.len() != 3 || widths.iter().any(|width| *width > MAX_XREF_FIELD_BYTES) {
        return Err(invalid());
    }
    let width = widths.iter().sum::<usize>();
    if width == 0 {
        return Err(invalid());
    }
    let size = xref_size(trailer, budget)?;
    let ranges = probe_dict_value(trailer, b"Index");
    let capacity = data.capacity();
    let mut data = decode_predictor(&mut data, trailer, width, budget)?;
    let mut entries = HashMap::new();
    let mut ranges = match ranges {
        Some(value) => XrefRanges::Array(value.strip_prefix(b"[").ok_or_else(invalid)?),
        None => XrefRanges::All(Some(size)),
    };
    while let Some((first_row, rows)) = ranges.next()? {
        for number in row_range(first_row, rows, entries.len(), budget)? {
            budget.check_deadline()?;
            let (row, rest) = data.split_at_checked(width).ok_or_else(invalid)?;
            data = rest;
            let (kind, row) = row.split_at(widths[0]);
            let (first, second) = row.split_at(widths[1]);
            let first = usize::try_from(big_endian(first)).map_err(|_| invalid())?;
            let second = big_endian(second);
            let entry = match if kind.is_empty() { 1 } else { big_endian(kind) } {
                0 => Entry::Free,
                1 => Entry::Direct {
                    offset: first,
                    generation: u16::try_from(second).map_err(|_| invalid())?,
                },
                2 => Entry::Packed {
                    stream: u32::try_from(first).map_err(|_| invalid())?,
                    member: usize::try_from(second).map_err(|_| invalid())?,
                },
                _ => return Err(invalid()),
            };
            insert(&mut entries, number, entry, budget)?;
        }
    }
    if !data.is_empty() {
        return Err(invalid());
    }
    budget.release(capacity);
    verify_size(trailer, &entries, budget)?;
    Ok(Section { entries, trailer })
}

/// Xref streams can have a byte-oriented PNG predictor, one scanline per row.
fn decode_predictor<'a>(
    data: &'a mut Vec<u8>,
    trailer: &[u8],
    width: usize,
    budget: &PdfDecodeBudget,
) -> Result<&'a [u8]> {
    let Some(params) = probe_dict_value(trailer, b"DecodeParms") else {
        return Ok(data);
    };
    let params = dict_span(params).ok_or_else(invalid)?;
    let predictor = optional_uint(params, b"Predictor")?.unwrap_or(1);
    if predictor == 1 {
        return Ok(data);
    }
    if !(10..=15).contains(&predictor)
        || optional_uint(params, b"Columns")?.unwrap_or(1) != width
        || optional_uint(params, b"Colors")?.unwrap_or(1) != 1
        || optional_uint(params, b"BitsPerComponent")?.unwrap_or(8) != 8
        || !data.len().is_multiple_of(width + 1)
    {
        return Err(invalid());
    }
    let rows = data.len() / (width + 1);
    for row in 0..rows {
        budget.check_deadline()?;
        let input = row * (width + 1);
        let output = row * width;
        let filter = data[input];
        for column in 0..width {
            let left = if column == 0 {
                0
            } else {
                data[output + column - 1]
            };
            let up = if row == 0 {
                0
            } else {
                data[output - width + column]
            };
            let upper_left = if row == 0 || column == 0 {
                0
            } else {
                data[output - width + column - 1]
            };
            let delta = match filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => left.midpoint(up),
                4 => paeth(left, up, upper_left),
                _ => return Err(invalid()),
            };
            data[output + column] = data[input + 1 + column].wrapping_add(delta);
        }
    }
    data.truncate(rows * width);
    Ok(data)
}

fn paeth(left: u8, up: u8, upper_left: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(upper_left);
    [left, up, upper_left]
        .into_iter()
        .min_by_key(|value| (estimate - i16::from(*value)).abs())
        .expect("three predictor candidates")
}

fn load_objects<'a>(
    pdf: &'a [u8],
    entries: &HashMap<u32, Entry>,
    sections: &[usize],
    budget: &mut PdfDecodeBudget,
) -> Result<Objects<'a>> {
    let mut direct = Vec::new();
    let mut packed = Vec::new();
    for (&number, entry) in entries {
        budget.visit(1)?;
        match *entry {
            Entry::Direct { offset, generation } => {
                budget.reserve_vec(&mut direct, 1)?;
                direct.push((offset, PdfRef { number, generation }));
            }
            Entry::Packed { stream, member } => {
                budget.reserve_vec(&mut packed, 1)?;
                packed.push((stream, number, member));
            }
            Entry::Free => {}
        }
    }
    direct.sort_unstable_by_key(|(offset, _)| *offset);
    let mut objects = Objects::default();
    for (index, &(offset, expected)) in direct.iter().enumerate() {
        budget.visit(1)?;
        let next_section = sections.get(sections.partition_point(|position| *position <= offset));
        let bound = direct
            .get(index + 1)
            .map_or(pdf.len(), |(offset, _)| *offset)
            .min(next_section.copied().unwrap_or(pdf.len()))
            .min(pdf.len());
        let (actual, start) = header(&pdf[..bound], offset).ok_or_else(invalid)?;
        if actual != expected {
            return Err(invalid());
        }
        let object = object_at(pdf, start..bound, actual.generation, entries, budget)?;
        budget.retain(std::mem::size_of::<(u32, PdfObject<'_>)>())?;
        objects.direct.insert(actual.number, object);
    }
    budget.release(direct.capacity() * std::mem::size_of::<(usize, PdfRef)>());
    packed.sort_unstable();
    for members in packed.chunk_by(|left, right| left.0 == right.0) {
        let stream = members[0].0;
        budget.visit(1)?;
        let Some((data, offsets, first)) = load_packed_container(&objects.direct, stream, budget)?
        else {
            if budget.deadline.is_some() {
                return Err(invalid());
            }
            for &(_, number, _) in members {
                objects.insert_packed(number, PackedObject::Unsupported, budget)?;
            }
            continue;
        };
        for &(_, number, member) in members {
            budget.visit(1)?;
            let &(actual, offset) = offsets.get(member).ok_or_else(invalid)?;
            if actual != number {
                return Err(invalid());
            }
            let range = member_range(stream, &data, &offsets, first, member, offset);
            if budget.deadline.is_some() {
                let PackedObject::Member { start, end, .. } = range else {
                    unreachable!()
                };
                budget.retain(end - start)?;
                objects.direct.insert(
                    number,
                    PdfObject::new(Cow::Owned(data[start..end].to_vec()), 0, None),
                );
            } else {
                objects.insert_packed(number, range, budget)?;
            }
        }
        budget.release(data.capacity() + offsets.capacity() * std::mem::size_of::<(u32, usize)>());
    }
    budget.release(packed.capacity() * std::mem::size_of::<(u32, u32, usize)>());
    Ok(objects)
}

fn object_at<'a>(
    document: &'a [u8],
    bounds: Range<usize>,
    generation: u16,
    entries: &HashMap<u32, Entry>,
    budget: &mut PdfDecodeBudget,
) -> Result<PdfObject<'a>> {
    budget.check_deadline()?;
    let pdf = &document[..bounds.end];
    let start = skip_pdf_space_and_comments(pdf, bounds.start);
    let (end, stream) = if let Some(dict) = direct_dict(pdf, start) {
        let cursor = skip_pdf_space_and_comments(pdf, start + dict.len());
        if pdf_keyword_at(pdf, cursor, b"stream") {
            let length = stream_length(dict, document, entries)?;
            let after = pdf.get(cursor + 6..).ok_or_else(invalid)?;
            let eol = if after.starts_with(b"\r\n") {
                2
            } else if after.starts_with(b"\n") {
                1
            } else {
                return Err(invalid());
            };
            let payload = cursor + 6 + eol;
            let end = payload
                .checked_add(length)
                .filter(|end| *end <= pdf.len())
                .ok_or_else(invalid)?;
            let endstream = skip_pdf_space_and_comments(pdf, end);
            if !pdf_keyword_at(pdf, endstream, b"endstream") {
                return Err(invalid());
            }
            (
                skip_pdf_space_and_comments(pdf, endstream + 9),
                Some(payload - start..end - start),
            )
        } else {
            (cursor, None)
        }
    } else {
        (find_endobj(pdf, start).ok_or_else(invalid)?, None)
    };
    if !pdf_keyword_at(pdf, end, b"endobj") {
        return Err(invalid());
    }
    budget.check_deadline()?;
    let body = pdf.get(start..end).ok_or_else(invalid)?;
    budget.scan(body.len())?;
    Ok(PdfObject::new(Cow::Borrowed(body), generation, stream))
}

fn stream_length(dict: &[u8], pdf: &[u8], entries: &HashMap<u32, Entry>) -> Result<usize> {
    if let Some(reference) = probe_dict_ref(dict, b"Length") {
        let Some(Entry::Direct { offset, generation }) = entries.get(&reference.number).copied()
        else {
            return Err(invalid());
        };
        let (actual, start) = header(pdf, offset).ok_or_else(invalid)?;
        if actual != reference || generation != reference.generation {
            return Err(invalid());
        }
        let (length, end) = unsigned(pdf, start).ok_or_else(invalid)?;
        if !pdf_keyword_at(pdf, skip_pdf_space_and_comments(pdf, end), b"endobj") {
            return Err(invalid());
        }
        return Ok(length);
    }
    optional_uint(dict, b"Length")?.ok_or_else(invalid)
}

pub(super) fn content_references(dict: &[u8], budget: &PdfDecodeBudget) -> Result<Vec<PdfRef>> {
    let Some(value) = probe_dict_value(dict, b"Contents") else {
        return Ok(Vec::new());
    };
    let Some(array) = value.strip_prefix(b"[") else {
        return parse_ref(value, 0)
            .map(|(reference, _)| vec![reference])
            .ok_or_else(invalid);
    };
    let mut cursor = skip_pdf_space_and_comments(array, 0);
    let mut references = Vec::new();
    while array.get(cursor) != Some(&b']') {
        budget.check_deadline()?;
        if references.len() == MAX_PROBE_OBJECTS {
            return Err(invalid());
        }
        let (reference, next) = parse_ref(array, cursor).ok_or_else(invalid)?;
        references.push(reference);
        cursor = skip_pdf_space_and_comments(array, next);
    }
    Ok(references)
}

fn header(pdf: &[u8], offset: usize) -> Option<(PdfRef, usize)> {
    if !pdf.get(offset)?.is_ascii_digit()
        || (offset > 0 && !pdf[offset - 1].is_ascii_whitespace() && pdf[offset - 1] != 0)
    {
        return None;
    }
    let (number, next) = unsigned(pdf, offset)?;
    let (generation, next) = unsigned(pdf, next)?;
    let keyword = skip_pdf_space_and_comments(pdf, next);
    if !pdf_keyword_at(pdf, keyword, b"obj") {
        return None;
    }
    Some((
        PdfRef {
            number: u32::try_from(number).ok()?,
            generation: u16::try_from(generation).ok()?,
        },
        keyword + 3,
    ))
}

fn direct_dict(pdf: &[u8], from: usize) -> Option<&[u8]> {
    let tail = pdf.get(skip_pdf_space_and_comments(pdf, from)..)?;
    tail.strip_prefix(b"<<")?;
    dict_span(tail)
}

fn verify_size(
    trailer: &[u8],
    entries: &HashMap<u32, Entry>,
    budget: &PdfDecodeBudget,
) -> Result<()> {
    let size = xref_size(trailer, budget)?;
    if entries.is_empty() || entries.keys().any(|number| *number as usize >= size) {
        return Err(invalid());
    }
    Ok(())
}

fn xref_size(trailer: &[u8], budget: &PdfDecodeBudget) -> Result<usize> {
    optional_uint(trailer, b"Size")?
        .filter(|size| *size > 0 && (budget.deadline.is_none() || *size <= MAX_PROBE_OBJECTS))
        .ok_or_else(invalid)
}

fn optional_uint(dict: &[u8], key: &[u8]) -> Result<Option<usize>> {
    probe_dict_value(dict, key)
        .map(|value| {
            let (number, end) = unsigned(value, 0).ok_or_else(invalid)?;
            if value.get(end).is_some_and(|byte| !is_pdf_delimiter(*byte)) {
                return Err(invalid());
            }
            Ok(number)
        })
        .transpose()
}

fn uint_array(bytes: &[u8], limit: usize) -> Option<Vec<usize>> {
    let bytes = bytes.strip_prefix(b"[")?;
    let mut values = Vec::new();
    let mut cursor = skip_pdf_space_and_comments(bytes, 0);
    while bytes.get(cursor) != Some(&b']') {
        if values.len() == limit {
            return None;
        }
        let (value, next) = unsigned(bytes, cursor)?;
        values.push(value);
        cursor = skip_pdf_space_and_comments(bytes, next);
    }
    Some(values)
}

enum XrefRanges<'a> {
    All(Option<usize>),
    Array(&'a [u8]),
}

impl XrefRanges<'_> {
    fn next(&mut self) -> Result<Option<(usize, usize)>> {
        match self {
            Self::All(size) => Ok(size.take().map(|size| (0, size))),
            Self::Array(rest) => {
                let cursor = skip_pdf_space_and_comments(rest, 0);
                if rest.get(cursor) == Some(&b']') {
                    return Ok(None);
                }
                let (first, after) = unsigned(rest, cursor).ok_or_else(invalid)?;
                let (size, after) = unsigned(rest, after).ok_or_else(invalid)?;
                *rest = &rest[after..];
                Ok(Some((first, size)))
            }
        }
    }
}

fn row_range(
    first: usize,
    count: usize,
    loaded: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<Range<u32>> {
    let end = first
        .checked_add(count)
        .and_then(|end| u32::try_from(end).ok())
        .ok_or_else(invalid)?;
    if count == 0
        || (budget.deadline.is_some()
            && (end as usize > MAX_PROBE_OBJECTS || count > MAX_PROBE_OBJECTS - loaded))
    {
        return Err(invalid());
    }
    budget.visit(count)?;
    Ok(first as u32..end)
}

fn insert(
    entries: &mut HashMap<u32, Entry>,
    number: u32,
    entry: Entry,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    if (number == 0 && !matches!(entry, Entry::Free)) || entries.contains_key(&number) {
        return Err(invalid());
    }
    budget.retain(std::mem::size_of::<(u32, Entry)>())?;
    entries.insert(number, entry);
    Ok(())
}

fn unsigned(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let from = skip_pdf_space_and_comments(bytes, from);
    let mut end = from;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
        if end - from > 20 {
            return None;
        }
    }
    Some((decimal(bytes.get(from..end)?)?, end))
}

fn decimal(bytes: &[u8]) -> Option<usize> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn big_endian(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

pub(super) fn invalid() -> AdapterError {
    AdapterError::ContractViolation("invalid or unsupported PDF cross-reference data".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xref_offset_must_point_to_the_start_of_the_object_number() {
        let pdf = b"%PDF-1.6\n13 0 obj\n<< /Type /Page >>\nendobj\n";
        let start = pdf
            .windows(8)
            .position(|bytes| bytes == b"13 0 obj")
            .unwrap();
        assert_eq!(header(pdf, start).unwrap().0.number, 13);
        assert!(header(pdf, start + 1).is_none());
    }
}
