//! Resolve active cross-reference entries in PDF revisions.
use super::{
    AdapterError, MAX_PROBE_OBJECTS, ObjectIndex, Objects, PackedObject, PdfDecodeBudget,
    PdfObject, PdfRef, Result, dict_span, effective_stream, find_endobj, is_pdf_delimiter,
    load_packed_container, member_range, parse_ref, pdf_keyword_at, pdf_name_end, probe_dict_ref,
    probe_dict_type, probe_dict_value, skip_pdf_literal_string, skip_pdf_space_and_comments,
};
use std::borrow::Cow;
use std::cell::Cell;
use std::cmp::Reverse;
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
    entries: Vec<(u32, Entry)>,
    trailer: &'a [u8],
}

struct XrefRow {
    number: u32,
    entry: Entry,
    revision: usize,
    supplemental: bool,
}

fn capacity_bytes<T>(entries: &Vec<T>) -> usize {
    entries.capacity() * std::mem::size_of::<T>()
}

fn release_index<T>(entries: Vec<T>, budget: &mut PdfDecodeBudget) {
    let bytes = capacity_bytes(&entries);
    drop(entries);
    budget.release(bytes);
}

/// Keep the old allocation charged until its replacement has been admitted.
fn reserve_index<T>(
    entries: &mut Vec<T>,
    additional: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let needed = entries.len().checked_add(additional).ok_or_else(invalid)?;
    if needed <= entries.capacity() {
        return Ok(());
    }
    let width = std::mem::size_of::<T>();
    let available = (budget.memory_limit - budget.retained - budget.cached_bytes()) / width;
    let next = needed.max(entries.capacity().saturating_mul(2).min(available));
    let bytes = next.checked_mul(width).ok_or_else(invalid)?;
    let old = capacity_bytes(entries);
    budget.retain(bytes)?;
    if entries.try_reserve_exact(next - entries.len()).is_err() {
        let error = budget.allocation_error();
        budget.release(bytes);
        return Err(error);
    }
    budget.release(old);
    Ok(())
}

pub(super) fn probe_objects<'a>(
    pdf: &'a [u8],
    budget: &mut PdfDecodeBudget,
) -> Result<(PdfRef, ObjectIndex<PdfObject<'a>>)> {
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
    let mut rows = Vec::new();
    let mut section_offsets = Vec::new();
    let mut revisions = Vec::new();
    while let Some(position) = offset {
        budget.visit(1)?;
        push_section(&mut section_offsets, position, budget)?;
        reserve_index(&mut revisions, 1, budget)?;
        revisions.push(position);
        let section = if pdf_keyword_at(pdf, position, b"xref") {
            classic_section(pdf, position, budget)?
        } else {
            stream_section(pdf, position, budget)?
        };
        if let Some(supplement) = optional_uint(section.trailer, b"XRefStm")? {
            if !pdf_keyword_at(pdf, position, b"xref") || supplement >= position {
                return Err(invalid());
            }
            push_section(&mut section_offsets, supplement, budget)?;
            let supplemental = stream_section(pdf, supplement, budget)?;
            append_section(&mut rows, supplemental.entries, position, true, budget)?;
        }
        if probe_dict_value(section.trailer, b"Encrypt").is_some() {
            return Err(invalid());
        }
        if root.is_none() && probe_dict_value(section.trailer, b"Root").is_some() {
            root = Some(probe_dict_ref(section.trailer, b"Root").ok_or_else(invalid)?);
        }
        append_section(&mut rows, section.entries, position, false, budget)?;
        offset = optional_uint(section.trailer, b"Prev")?;
        if offset.is_some_and(|previous| previous >= position) {
            return Err(invalid());
        }
    }
    section_offsets.sort_unstable();
    section_offsets.dedup();
    revisions.reverse();
    rows.sort_unstable_by_key(|row| (row.number, Reverse(row.revision), Reverse(row.supplemental)));
    let objects = load_objects(pdf, &rows, &section_offsets, &revisions, budget)?;
    budget.release(
        capacity_bytes(&section_offsets) + capacity_bytes(&revisions) + capacity_bytes(&rows),
    );
    Ok((root.ok_or_else(invalid)?, objects))
}

fn push_section(
    offsets: &mut Vec<usize>,
    position: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    if budget.deadline.is_some() && offsets.len() == MAX_XREF_SECTIONS {
        return Err(invalid());
    }
    reserve_index(offsets, 1, budget)?;
    offsets.push(position);
    Ok(())
}

fn append_section(
    rows: &mut Vec<XrefRow>,
    entries: Vec<(u32, Entry)>,
    revision: usize,
    supplemental: bool,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    let count = entries.len();
    let capacity = capacity_bytes(&entries);
    if budget.deadline.is_some() && rows.len() + count > MAX_PROBE_OBJECTS {
        return Err(invalid());
    }
    budget.visit(count)?;
    reserve_index(rows, count, budget)?;
    rows.extend(entries.into_iter().map(|(number, entry)| XrefRow {
        number,
        entry,
        revision,
        supplemental,
    }));
    budget.release(capacity);
    Ok(())
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
    let mut entries = Vec::new();
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
    verify_section(trailer, &mut entries, budget)?;
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
    let (definition, _) = object_at(
        pdf,
        start,
        reference.generation,
        &ObjectLocations::default(),
        usize::MAX,
        budget,
    )?;
    let mut data =
        effective_stream(&definition, &ObjectIndex::default(), budget)?.ok_or_else(invalid)?;
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
    let mut entries = Vec::new();
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
    verify_section(trailer, &mut entries, budget)?;
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

struct PhysicalObject {
    offset: usize,
    reference: PdfRef,
    active: bool,
    at_boundary: Cell<bool>,
    length: Cell<Option<usize>>,
}

impl PhysicalObject {
    fn key(&self) -> (usize, u32, u16) {
        (
            self.offset,
            self.reference.number,
            self.reference.generation,
        )
    }
}

#[derive(Default)]
struct ObjectLocations<'a> {
    rows: &'a [XrefRow],
    physical: &'a [PhysicalObject],
}

impl ObjectLocations<'_> {
    fn find(&self, number: u32, revision: usize) -> Option<&XrefRow> {
        let index = self.rows.partition_point(|row| {
            row.number < number || (row.number == number && row.revision > revision)
        });
        self.rows.get(index).filter(|row| row.number == number)
    }

    fn physical(&self, offset: usize, reference: PdfRef) -> Option<&PhysicalObject> {
        let key = (offset, reference.number, reference.generation);
        let index = self
            .physical
            .binary_search_by_key(&key, PhysicalObject::key)
            .ok()?;
        self.physical.get(index)
    }
}

fn load_objects<'a>(
    pdf: &'a [u8],
    rows: &[XrefRow],
    sections: &[usize],
    revisions: &[usize],
    budget: &mut PdfDecodeBudget,
) -> Result<Objects<'a>> {
    let mut physical = Vec::new();
    for group in rows.chunk_by(|left, right| left.number == right.number) {
        for (index, row) in group.iter().enumerate() {
            budget.visit(1)?;
            if let Entry::Direct { offset, generation } = row.entry {
                reserve_index(&mut physical, 1, budget)?;
                physical.push(PhysicalObject {
                    offset,
                    reference: PdfRef {
                        number: row.number,
                        generation,
                    },
                    active: index == 0,
                    at_boundary: Cell::new(false),
                    length: Cell::new(None),
                });
            }
        }
    }
    physical.sort_unstable_by_key(PhysicalObject::key);
    physical.dedup_by(|later, earlier| {
        if later.key() != earlier.key() {
            return false;
        }
        earlier.active |= later.active;
        true
    });
    let locations = ObjectLocations {
        rows,
        physical: &physical,
    };
    let mut objects = Objects::default();
    let mut cursor = 0;
    let mut sections = sections.iter().copied().peekable();
    for expected in physical.iter().filter(|object| object.active) {
        while let Some(position) = sections.next_if(|position| *position < expected.offset) {
            cursor = scan_to_boundary(pdf, cursor, position, &locations, revisions, budget)?;
        }
        cursor = scan_to_boundary(pdf, cursor, expected.offset, &locations, revisions, budget)?;
        let (actual, start) = header(pdf, cursor).ok_or_else(invalid)?;
        if actual != expected.reference {
            return Err(invalid());
        }
        let (object, end) = object_at(
            pdf,
            start,
            actual.generation,
            &locations,
            usize::MAX,
            budget,
        )?;
        cursor = end;
        sections.next_if_eq(&expected.offset);
        expected.at_boundary.set(true);
        reserve_index(&mut objects.direct.sorted, 1, budget)?;
        objects.direct.sorted.push((actual.number, object));
    }
    for position in sections {
        cursor = scan_to_boundary(pdf, cursor, position, &locations, revisions, budget)?;
    }
    // Lexical scans may use a historical /Length to skip an inactive stream.
    // Any length used to establish boundaries must itself occur at a boundary.
    if physical
        .iter()
        .any(|object| object.length.get().is_some() && !object.at_boundary.get())
    {
        return Err(invalid());
    }
    release_index(physical, budget);
    objects
        .direct
        .sorted
        .sort_unstable_by_key(|(number, _)| *number);

    let mut packed = Vec::new();
    for group in rows.chunk_by(|left, right| left.number == right.number) {
        budget.check_deadline()?;
        let row = &group[0];
        if let Entry::Packed { stream, member } = row.entry {
            reserve_index(&mut packed, 1, budget)?;
            packed.push((stream, row.number, member));
        }
    }
    packed.sort_unstable();
    let mut probe_members = Vec::new();
    for members in packed.chunk_by(|left, right| left.0 == right.0) {
        let stream = members[0].0;
        budget.visit(1)?;
        let Some((data, offsets, first)) = load_packed_container(&objects.direct, stream, budget)?
        else {
            if budget.deadline.is_some() {
                return Err(invalid());
            }
            reserve_index(&mut objects.packed.sorted, members.len(), budget)?;
            objects.packed.sorted.extend(
                members
                    .iter()
                    .map(|&(_, number, _)| (number, PackedObject::Unsupported)),
            );
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
                reserve_index(&mut probe_members, 1, budget)?;
                budget.retain(end - start)?;
                probe_members.push((
                    number,
                    PdfObject::new(Cow::Owned(data[start..end].to_vec()), 0, None),
                ));
            } else {
                reserve_index(&mut objects.packed.sorted, 1, budget)?;
                objects.packed.sorted.push((number, range));
            }
        }
        budget.release(data.capacity() + offsets.capacity() * std::mem::size_of::<(u32, usize)>());
    }
    release_index(packed, budget);
    objects
        .packed
        .sorted
        .sort_unstable_by_key(|(number, _)| *number);
    if !probe_members.is_empty() {
        let capacity = capacity_bytes(&probe_members);
        reserve_index(&mut objects.direct.sorted, probe_members.len(), budget)?;
        objects.direct.sorted.extend(probe_members);
        budget.release(capacity);
        objects
            .direct
            .sorted
            .sort_unstable_by_key(|(number, _)| *number);
    }
    Ok(objects)
}

/// Advance over complete lexical objects, including inactive revisions. A
/// requested offset skipped by a comment, string or stream is never a header.
fn scan_to_boundary(
    pdf: &[u8],
    mut cursor: usize,
    target: usize,
    locations: &ObjectLocations<'_>,
    revisions: &[usize],
    budget: &mut PdfDecodeBudget,
) -> Result<usize> {
    if target >= pdf.len() {
        return Err(invalid());
    }
    while cursor < target {
        budget.check_deadline()?;
        cursor = skip_pdf_space_and_comments(pdf, cursor);
        if cursor >= target {
            break;
        }
        if let Some((reference, start)) = header(pdf, cursor) {
            let revision = revisions
                .get(revisions.partition_point(|offset| *offset < cursor))
                .copied()
                .unwrap_or(usize::MAX);
            let (_, end) = object_at(
                pdf,
                start,
                reference.generation,
                locations,
                revision,
                budget,
            )?;
            if let Some(object) = locations.physical(cursor, reference) {
                object.at_boundary.set(true);
            }
            cursor = end;
        } else {
            cursor = match pdf[cursor] {
                b'(' => skip_pdf_literal_string(pdf, cursor),
                b'<' if pdf.get(cursor + 1) == Some(&b'<') => {
                    cursor + direct_dict(pdf, cursor).ok_or_else(invalid)?.len()
                }
                b'<' => pdf[cursor + 1..]
                    .iter()
                    .position(|byte| *byte == b'>')
                    .map_or(pdf.len(), |end| cursor + 2 + end),
                b'/' => pdf_name_end(pdf, cursor + 1),
                0 => cursor + 1,
                value if is_pdf_delimiter(value) => cursor + 1,
                _ => pdf_name_end(pdf, cursor + 1),
            };
        }
    }
    if cursor == target {
        Ok(cursor)
    } else {
        Err(invalid())
    }
}

fn object_at<'a>(
    pdf: &'a [u8],
    body_start: usize,
    generation: u16,
    locations: &ObjectLocations<'_>,
    revision: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<(PdfObject<'a>, usize)> {
    budget.check_deadline()?;
    let start = skip_pdf_space_and_comments(pdf, body_start);
    let (end, stream) = if let Some(dict) = direct_dict(pdf, start) {
        let cursor = skip_pdf_space_and_comments(pdf, start + dict.len());
        if pdf_keyword_at(pdf, cursor, b"stream") {
            let length = stream_length(dict, pdf, locations, revision, budget)?;
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
    budget.scan(end + b"endobj".len() - body_start)?;
    let mut object = PdfObject::new(Cow::Borrowed(body), generation, stream);
    object.stream_length_checked = true;
    Ok((object, end + b"endobj".len()))
}

fn stream_length(
    dict: &[u8],
    pdf: &[u8],
    locations: &ObjectLocations<'_>,
    revision: usize,
    budget: &mut PdfDecodeBudget,
) -> Result<usize> {
    let Some(reference) = probe_dict_ref(dict, b"Length") else {
        return optional_uint(dict, b"Length")?.ok_or_else(invalid);
    };
    budget.visit(1)?;
    let Some(&XrefRow {
        entry: Entry::Direct { offset, generation },
        ..
    }) = locations.find(reference.number, revision)
    else {
        return Err(invalid());
    };
    if generation != reference.generation {
        return Err(invalid());
    }
    let physical = locations.physical(offset, reference).ok_or_else(invalid)?;
    if let Some(length) = physical.length.get() {
        return Ok(length);
    }
    let (actual, start) = header(pdf, offset).ok_or_else(invalid)?;
    if actual != reference {
        return Err(invalid());
    }
    let (length, end) = unsigned(pdf, start).ok_or_else(invalid)?;
    let terminator = skip_pdf_space_and_comments(pdf, end);
    if !pdf_keyword_at(pdf, terminator, b"endobj") {
        return Err(invalid());
    }
    budget.scan(terminator + b"endobj".len() - offset)?;
    physical.length.set(Some(length));
    Ok(length)
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

fn verify_section(
    trailer: &[u8],
    entries: &mut [(u32, Entry)],
    budget: &PdfDecodeBudget,
) -> Result<()> {
    let size = xref_size(trailer, budget)?;
    entries.sort_unstable_by_key(|(number, _)| *number);
    if !entries
        .last()
        .is_some_and(|(number, _)| (*number as usize) < size)
        || entries.windows(2).any(|pair| pair[0].0 == pair[1].0)
    {
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
    entries: &mut Vec<(u32, Entry)>,
    number: u32,
    entry: Entry,
    budget: &mut PdfDecodeBudget,
) -> Result<()> {
    if number == 0 && !matches!(entry, Entry::Free) {
        return Err(invalid());
    }
    reserve_index(entries, 1, budget)?;
    entries.push((number, entry));
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

    fn write_classic_xref(pdf: &mut Vec<u8>, offsets: &[usize]) {
        use std::io::Write;
        let table = pdf.len();
        let size = offsets.len() + 1;
        write!(pdf, "xref\n0 {size}\n0000000000 65535 f \n").unwrap();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        write!(
            pdf,
            "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{table}\n%%EOF\n"
        )
        .unwrap();
    }

    #[test]
    fn p1_review2_xref_rejects_headers_in_comments_and_inactive_streams() {
        for wrap_stream in [false, true] {
            let mut pdf = b"%PDF-1.6\n".to_vec();
            let root = pdf.len();
            pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
            let pages = pdf.len();
            pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n");
            let forged = b"3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n";
            if wrap_stream {
                pdf.extend_from_slice(
                    format!("4 0 obj << /Length {} >> stream\n", forged.len()).as_bytes(),
                );
            } else {
                pdf.extend_from_slice(b"% ");
            }
            let page = pdf.len();
            pdf.extend_from_slice(forged);
            if wrap_stream {
                pdf.extend_from_slice(b"endstream\nendobj\n");
            }
            write_classic_xref(&mut pdf, &[root, pages, page]);
            assert!(
                matches!(
                    super::super::PdfPages::open(&pdf, kio_core::error::ResourceTier::Standard),
                    Err(AdapterError::ContractViolation(_))
                ),
                "wrapped in stream: {wrap_stream}"
            );
        }
    }

    #[test]
    fn xref_rejects_object_offsets_beyond_the_file() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        let invalid = pdf.len() + 4096;
        write_classic_xref(&mut pdf, &[invalid]);
        assert!(matches!(
            super::super::PdfPages::open(&pdf, kio_core::error::ResourceTier::Standard),
            Err(AdapterError::ContractViolation(_))
        ));
    }

    #[test]
    fn pdf_xref_discards_superseded_offsets_but_checks_active_offsets() {
        use std::io::Write;

        for bad_offset in [0, b"%PDF-1.6\n".len(), 4096] {
            let mut pdf = b"%PDF-1.6\n".to_vec();
            let root = pdf.len();
            pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
            let pages = pdf.len();
            pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n");
            let page = pdf.len();
            pdf.extend_from_slice(
                b"3 0 obj << /Type /Page /Parent 2 0 R /Contents 5 0 R >> endobj\n",
            );
            let content = pdf.len();
            pdf.extend_from_slice(
                b"5 0 obj << /Length 4 0 R >> stream\nBT (one) Tj\nendstream\nendobj\n",
            );
            let length = pdf.len();
            pdf.extend_from_slice(b"4 0 obj 11 endobj\n");
            let previous = pdf.len();
            write_classic_xref(&mut pdf, &[root, pages, bad_offset, bad_offset, content]);

            for latest in [page, bad_offset] {
                let mut updated = pdf.clone();
                let xref = updated.len();
                write!(
                    updated,
                    "xref\n3 2\n{latest:010} 00000 n \n{length:010} 00000 n \ntrailer\n<< /Size 6 /Prev {previous} >>\nstartxref\n{xref}\n%%EOF\n"
                )
                .unwrap();
                let result =
                    super::super::PdfPages::open(&updated, kio_core::error::ResourceTier::Standard);
                if latest == page {
                    assert!(
                        result.is_ok(),
                        "superseded offset {bad_offset}: {:?}",
                        result.err()
                    );
                } else {
                    assert!(matches!(result, Err(AdapterError::ContractViolation(_))));
                }
            }
        }
    }

    #[test]
    fn pdf_xref_temporary_indexes_fit_in_separate_memory_phases() {
        use std::io::Write;

        let mut pdf = b"%PDF-1.6\n".to_vec();
        let mut rows = Vec::new();
        for number in 1..=3 {
            rows.push(XrefRow {
                number,
                entry: Entry::Direct {
                    offset: pdf.len(),
                    generation: 0,
                },
                revision: 0,
                supplemental: false,
            });
            if number == 1 {
                pdf.extend_from_slice(
                    b"1 0 obj << /Type /ObjStm /N 1 /First 4 /Length 8 >> stream\n5 0 null\nendstream\nendobj\n",
                );
            } else {
                writeln!(pdf, "{number} 0 obj null endobj").unwrap();
            }
        }
        rows.push(XrefRow {
            number: 5,
            entry: Entry::Packed {
                stream: 1,
                member: 0,
            },
            revision: 0,
            supplemental: false,
        });

        let physical = 4 * std::mem::size_of::<PhysicalObject>();
        let direct_width = std::mem::size_of::<(u32, PdfObject<'_>)>();
        // At the direct-index peak its old two slots and three new slots are charged.
        let peak = physical + 5 * direct_width;
        let mut budget = PdfDecodeBudget {
            memory_limit: peak - 1,
            ..PdfDecodeBudget::default()
        };
        assert!(matches!(
            load_objects(&pdf, &rows, &[], &[], &mut budget),
            Err(AdapterError::ResourceLimit(ref limit)) if limit.resource == kio_core::error::Resource::Memory
        ));

        let mut budget = PdfDecodeBudget {
            memory_limit: peak,
            ..PdfDecodeBudget::default()
        };
        let objects = load_objects(&pdf, &rows, &[], &[], &mut budget).unwrap();
        assert!(matches!(
            objects.packed.get(&5),
            Some(PackedObject::Member { .. })
        ));
        assert_eq!(
            budget.retained,
            capacity_bytes(&objects.direct.sorted) + capacity_bytes(&objects.packed.sorted)
        );
    }

    #[test]
    fn p1_review2_shared_length_charges_once_and_counts_each_reference() {
        let mut pdf = b"%PDF-1.6\n".to_vec();
        let offset = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n%");
        pdf.extend(std::iter::repeat_n(b'x', 4096));
        pdf.extend_from_slice(b"\n11\nendobj\n");
        let rows = [XrefRow {
            number: 3,
            entry: Entry::Direct {
                offset,
                generation: 0,
            },
            revision: 10,
            supplemental: false,
        }];
        let physical = [PhysicalObject {
            offset,
            reference: PdfRef {
                number: 3,
                generation: 0,
            },
            active: true,
            at_boundary: Cell::new(false),
            length: Cell::new(None),
        }];
        let locations = ObjectLocations {
            rows: &rows,
            physical: &physical,
        };
        let dict = b"<< /Length 3 0 R >>";
        let mut budget = PdfDecodeBudget {
            work_limit: 8,
            ..PdfDecodeBudget::default()
        };
        assert_eq!(
            stream_length(dict, &pdf, &locations, 10, &mut budget).unwrap(),
            11
        );
        let first = budget.work;
        assert!(first > 4);
        for _ in 0..2 {
            let before = budget.work;
            assert_eq!(
                stream_length(dict, &pdf, &locations, 10, &mut budget).unwrap(),
                11
            );
            assert_eq!(budget.work, before + 1);
        }
        assert!(matches!(
            stream_length(dict, &pdf, &locations, 10, &mut budget),
            Err(AdapterError::ResourceLimit(_))
        ));
    }

    #[test]
    fn p1_review2_xref_retains_allocated_index_capacity() {
        let pdf = b"xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 1 >>\n";
        let mut budget = PdfDecodeBudget::default();
        let section = classic_section(pdf, 0, &mut budget).unwrap();
        assert_eq!(
            budget.retained,
            section.entries.capacity() * std::mem::size_of::<(u32, Entry)>()
        );
    }

    #[test]
    fn p1_review2_xref_growth_admits_old_and_replacement_allocations() {
        let width = std::mem::size_of::<(u32, Entry)>();
        let mut budget = PdfDecodeBudget {
            memory_limit: 4 * width,
            ..PdfDecodeBudget::default()
        };
        let mut entries = Vec::new();
        insert(&mut entries, 0, Entry::Free, &mut budget).unwrap();
        insert(&mut entries, 1, Entry::Free, &mut budget).unwrap();

        let error = insert(&mut entries, 2, Entry::Free, &mut budget).unwrap_err();
        assert!(matches!(error, AdapterError::ResourceLimit(ref limit)
            if limit.resource == kio_core::error::Resource::Memory
                && limit.observed == Some((5 * width) as u64)));
        assert_eq!(entries.capacity(), 2);
        assert_eq!(budget.retained, 2 * width);

        budget.memory_limit = 5 * width;
        insert(&mut entries, 2, Entry::Free, &mut budget).unwrap();
        assert_eq!(entries.capacity(), 3);
        assert_eq!(budget.retained, 3 * width);
    }

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
