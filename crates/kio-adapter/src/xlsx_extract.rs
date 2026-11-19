//! Local XLSX extraction with one worksheet per prepared unit.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
};

use kio_core::error::ResourceTier;
use quick_xml::events::{BytesStart, Event};

use crate::{
    AdapterError, Result,
    ooxml_package::{BoundedZip, BoundedZipLimits, xml as opc},
};

mod budget;
mod xml;

use budget::Budget;
use xml::{DOCUMENT_REL_NS, Xml, reference_text};

const XLSX_MEDIA: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const WORKBOOK_CONTENT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
const SHEET_CONTENT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
const WORKSHEET_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
const SHARED_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";
const STYLES_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const MANIFEST: &str = "[Content_Types].xml";
const ROOT_RELS: &str = "_rels/.rels";

#[must_use]
pub fn is_xlsx_media(media_type: &str) -> bool {
    media_type == XLSX_MEDIA
}

#[must_use]
pub fn raw_zip_limit(tier: ResourceTier) -> u64 {
    BoundedZipLimits::xlsx(tier).raw_bytes()
}

pub(super) fn check_raw_zip_size(tier: ResourceTier, bytes: u64) -> Result<()> {
    BoundedZipLimits::xlsx(tier).check_raw_bytes(bytes)
}

struct SheetRef {
    name: String,
    target: String,
}

/// Workbook metadata and a cursor over the ZIP. Only the worksheet requested by
/// the caller is expanded and rendered. A verified durable prefix can be skipped.
pub struct XlsxWorkbook<'a> {
    archive: BoundedZip<'a>,
    budget: Budget,
    sheets: Vec<SheetRef>,
    shared: Vec<String>,
    formats: CellFormats,
}

impl<'a> XlsxWorkbook<'a> {
    pub fn open(bytes: &'a [u8], tier: ResourceTier) -> Result<Self> {
        let mut budget = Budget::new(tier);
        let mut archive = BoundedZip::open_accounted(
            bytes,
            BoundedZipLimits::xlsx(budget.tier),
            |directory, entries| {
                budget.visit()?;
                budget.expand(directory)?;
                // Allow for the archive's index, its copies of variable-length
                // header fields, and the paths kept for package validation.
                budget.retain(directory * 4 + entries * 512)
            },
        )?;
        let metadata_start = budget.retained();
        let main = with_part(&mut archive, &mut budget, ROOT_RELS, |bytes, budget| {
            Xml::validate(bytes)?;
            opc::office_document_target(bytes, |size| retain_record(budget, size))
        })?;
        let content = with_part(&mut archive, &mut budget, MANIFEST, |bytes, budget| {
            Xml::validate(bytes)?;
            opc::content_types(bytes, |size| retain_record(budget, size))
        })?;
        if content
            .overrides
            .get(&format!("/{main}"))
            .map(String::as_str)
            != Some(WORKBOOK_CONTENT)
        {
            return Err(invalid("workbook part has no XLSX content type"));
        }
        let pending = with_part(&mut archive, &mut budget, &main, parse_workbook_sheets)?;
        let relations_part = relationship_part(&main);
        let mut worksheets = HashMap::new();
        let mut shared_part = None;
        let mut styles_part = None;
        with_part(
            &mut archive,
            &mut budget,
            &relations_part,
            |bytes, budget| {
                Xml::validate(bytes)?;
                opc::relationships(
                    bytes,
                    |size| retain_record(budget, 2 * size + 2 * main.len()),
                    |id, kind, target, mode| {
                        let slot = match kind {
                            WORKSHEET_REL => {
                                if mode != "Internal" {
                                    return Err(invalid("worksheet relationship is external"));
                                }
                                worksheets.insert(id.to_owned(), resolve_target(&main, target)?);
                                return Ok(());
                            }
                            SHARED_REL => &mut shared_part,
                            STYLES_REL => &mut styles_part,
                            _ => return Ok(()),
                        };
                        if mode != "Internal" || slot.is_some() {
                            return Err(invalid(
                                "shared strings or styles relationship is ambiguous or external",
                            ));
                        }
                        *slot = Some(resolve_target(&main, target)?);
                        Ok(())
                    },
                )
            },
        )?;

        let mut targets = HashSet::new();
        let mut sheets = Vec::new();
        let mut sheets_memory = 0;
        for (name, id) in pending {
            budget.visit()?;
            let target = worksheets
                .remove(&id)
                .ok_or_else(|| invalid("sheet has no matching worksheet relationship"))?;
            if content.for_part(&target) != Some(SHEET_CONTENT) || !archive.contains(&target) {
                return Err(invalid(
                    "worksheet part is missing or has the wrong content type",
                ));
            }
            let charge = name.len() + target.len() + RECORD_BYTES;
            budget.retain(charge)?;
            sheets_memory += charge;
            sheets.push(SheetRef { name, target });
        }
        for sheet in &sheets {
            if !targets.insert(sheet.target.as_str()) {
                return Err(invalid("multiple sheets refer to the same worksheet part"));
            }
        }
        drop(worksheets);
        drop(content);

        // Check all other members through CRC-checked reads. XML outside the
        // extraction vocabulary is still checked for well-formedness.
        targets.extend([MANIFEST, ROOT_RELS, &main, &relations_part]);
        targets.extend(shared_part.as_deref());
        targets.extend(styles_part.as_deref());
        for index in 0..archive.member_names().len() {
            let name = archive.member_names()[index].clone();
            if targets.contains(name.as_str()) {
                continue;
            }
            if name.ends_with(".xml") || name.ends_with(".rels") {
                with_part(&mut archive, &mut budget, &name, |bytes, _| {
                    Xml::validate(bytes)
                })?;
            } else {
                archive.consume(&name, false, |length| {
                    budget.visit()?;
                    budget.expand(length)
                })?;
            }
        }
        drop(targets);
        drop(main);
        drop(relations_part);
        // Retire the bookkeeping for the manifest and temporary relationships.
        // The remaining part names are short-lived and already charged above.
        budget.release(budget.retained() - metadata_start - sheets_memory);
        let shared = match shared_part {
            Some(part) => with_part(&mut archive, &mut budget, &part, parse_shared_strings)?,
            None => Vec::new(),
        };
        let formats = match styles_part {
            Some(part) => with_part(&mut archive, &mut budget, &part, parse_styles)?,
            None => CellFormats::default(),
        };
        Ok(Self {
            archive,
            budget,
            sheets,
            shared,
            formats,
        })
    }

    pub fn sheet_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.sheets.iter().map(|sheet| sheet.name.as_str())
    }

    /// Embedded media is reported separately from extracted worksheet text.
    pub fn media_paths(&self) -> impl Iterator<Item = &str> {
        self.archive
            .member_names()
            .iter()
            .map(String::as_str)
            .filter(|name| name.starts_with("xl/media/") && !name.ends_with('/'))
    }

    /// Render one sheet. On exhaustion, the cursor identifies the last source
    /// row processed in that sheet. No partial Markdown is returned.
    pub fn read_sheet(&mut self, index: usize, through_row: &mut u64) -> Result<String> {
        *through_row = 0;
        let sheet = self
            .sheets
            .get(index)
            .ok_or_else(|| invalid("worksheet index is out of range"))?;
        let before = self.budget.retained();
        let result = with_part(
            &mut self.archive,
            &mut self.budget,
            &sheet.target,
            |bytes, budget| parse_sheet(bytes, &self.shared, &self.formats, budget, through_row),
        );
        self.budget.release(self.budget.retained() - before);
        result
    }
}

// Covers the owned strings' collection slots and index nodes.
const RECORD_BYTES: usize = 128;

fn retain_record(budget: &mut Budget, bytes: usize) -> Result<()> {
    budget.visit()?;
    budget.retain(RECORD_BYTES + bytes)
}

fn with_part<T>(
    archive: &mut BoundedZip<'_>,
    budget: &mut Budget,
    name: &str,
    parse: impl FnOnce(&[u8], &mut Budget) -> Result<T>,
) -> Result<T> {
    let part = archive
        .consume(name, true, |length| {
            budget.visit()?;
            budget.expand(length)?;
            // The source stays resident while the parser uses namespace and
            // attribute scratch space. Events otherwise borrow the source bytes.
            budget.retain(part_memory(length))
        })?
        .ok_or_else(|| invalid("required part is missing"))?;
    let result = parse(&part, budget);
    budget.release(part_memory(part.len()));
    result
}

fn part_memory(length: usize) -> usize {
    length * 2 + 128 * 1024
}

fn relationship_part(part: &str) -> String {
    part.rsplit_once('/').map_or_else(
        || format!("_rels/{part}.rels"),
        |(directory, file)| format!("{directory}/_rels/{file}.rels"),
    )
}

fn resolve_target(base: &str, target: &str) -> Result<String> {
    if target.contains(['\\', ':', '?', '#']) {
        return Err(invalid("relationship target is not an internal part path"));
    }
    let (parent, path) = match target.strip_prefix('/') {
        Some(absolute) => ("", absolute),
        None => (
            base.rsplit_once('/').map_or("", |(directory, _)| directory),
            target,
        ),
    };
    let mut resolved = parent.to_owned();
    for component in path.split('/') {
        match component {
            ".." if !resolved.is_empty() => {
                resolved.truncate(resolved.rfind('/').unwrap_or(0));
            }
            "" | "." | ".." => return Err(invalid("relationship target has an invalid path")),
            _ => {
                if !resolved.is_empty() {
                    resolved.push('/');
                }
                resolved.push_str(component);
            }
        }
    }
    Ok(resolved)
}

fn invalid(detail: &str) -> AdapterError {
    AdapterError::ContractViolation(format!("XLSX {detail}"))
}

fn xml_error(error: impl std::fmt::Display) -> AdapterError {
    AdapterError::ContractViolation(format!("XLSX XML is malformed: {error}"))
}

fn parse_workbook_sheets(bytes: &[u8], budget: &mut Budget) -> Result<Vec<(String, String)>> {
    let mut xml = Xml::new(bytes, Some(b"workbook"))?;
    let mut sheets = Vec::new();
    let mut in_sheets = false;
    loop {
        match xml.next()? {
            (1, Event::Start(tag)) if xml.sheet_element(tag.name(), b"sheets") => {
                in_sheets = true;
            }
            (1, Event::End(_)) => in_sheets = false,
            (2, Event::Start(tag) | Event::Empty(tag))
                if in_sheets && xml.sheet_element(tag.name(), b"sheet") =>
            {
                let name = xml
                    .attribute(&tag, b"", b"name")?
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| invalid("sheet has no name"))?;
                let id = xml
                    .attribute(&tag, DOCUMENT_REL_NS, b"id")?
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid("sheet has no relationship id"))?;
                retain_record(budget, name.len() + id.len())?;
                sheets.push((name, id));
            }
            (_, Event::Eof) if sheets.is_empty() => {
                return Err(invalid("workbook declares no sheets"));
            }
            (_, Event::Eof) => return Ok(sheets),
            _ => {}
        }
    }
}

fn parse_shared_strings(bytes: &[u8], budget: &mut Budget) -> Result<Vec<String>> {
    let mut xml = Xml::new(bytes, Some(b"sst"))?;
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_item = false;
    let mut run = false;
    let mut text_depth = None;
    loop {
        let (depth, event) = xml.next()?;
        if let Some(text) = text_content(&event, depth, text_depth)? {
            budget.retain(text.len() * 2)?;
            current.push_str(&text);
            continue;
        }
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag)
                if depth == 1 && xml.sheet_element(tag.name(), b"si") =>
            {
                retain_record(budget, 0)?;
                if empty {
                    strings.push(String::new());
                } else {
                    in_item = true;
                }
            }
            Event::Start(tag) if in_item && depth == 2 && xml.sheet_element(tag.name(), b"r") => {
                run = true
            }
            Event::Start(tag)
                if in_item
                    && (depth == 2 || (depth == 3 && run))
                    && xml.sheet_element(tag.name(), b"t") =>
            {
                text_depth = Some(depth + 1);
            }
            Event::End(_) if depth == 1 && in_item => {
                strings.push(std::mem::take(&mut current));
                in_item = false;
            }
            Event::End(_) => {
                if text_depth == Some(depth + 1) {
                    text_depth = None;
                }
                if depth == 2 {
                    run = false;
                }
            }
            Event::Eof => return Ok(strings),
            _ => {}
        }
    }
}

fn text_content<'a>(
    event: &'a Event<'_>,
    depth: usize,
    scope: Option<usize>,
) -> Result<Option<Cow<'a, str>>> {
    if scope != Some(depth) {
        return Ok(None);
    }
    match event {
        Event::Text(text) => text.decode().map(Some).map_err(xml_error),
        Event::CData(text) => text.decode().map(Some).map_err(xml_error),
        Event::GeneralRef(reference) => reference_text(reference).map(Some),
        _ => Ok(None),
    }
}

#[derive(Default)]
struct CellFormats {
    style_formats: Vec<u32>,
    custom: BTreeMap<u32, String>,
}

impl CellFormats {
    fn code_for_style(&self, style_index: Option<usize>) -> Option<&str> {
        let id = *self.style_formats.get(style_index?)?;
        self.custom
            .get(&id)
            .map(String::as_str)
            .or_else(|| builtin_number_format(id))
    }
}

fn parse_styles(bytes: &[u8], budget: &mut Budget) -> Result<CellFormats> {
    let mut xml = Xml::new(bytes, Some(b"styleSheet"))?;
    let mut formats = CellFormats::default();
    let mut in_custom = false;
    let mut in_cell_xfs = false;
    loop {
        let (depth, event) = xml.next()?;
        match event {
            Event::Start(tag) if depth == 1 => {
                in_custom = xml.sheet_element(tag.name(), b"numFmts");
                in_cell_xfs = xml.sheet_element(tag.name(), b"cellXfs");
            }
            Event::End(_) if depth == 1 => {
                in_custom = false;
                in_cell_xfs = false;
            }
            Event::Start(tag) | Event::Empty(tag) if depth == 2 => {
                if in_custom && xml.sheet_element(tag.name(), b"numFmt") {
                    let id =
                        format_id(&xml, &tag)?.ok_or_else(|| invalid("custom format has no id"))?;
                    let code = xml
                        .attribute(&tag, b"", b"formatCode")?
                        .ok_or_else(|| invalid("custom format has no code"))?;
                    retain_record(budget, code.len())?;
                    if formats.custom.insert(id, code).is_some() {
                        return Err(invalid("duplicate custom format id"));
                    }
                } else if in_cell_xfs && xml.sheet_element(tag.name(), b"xf") {
                    budget.visit()?;
                    budget.retain(2 * size_of::<u32>())?;
                    formats
                        .style_formats
                        .push(format_id(&xml, &tag)?.unwrap_or(0));
                }
            }
            Event::Eof => return Ok(formats),
            _ => {}
        }
    }
}

fn format_id(xml: &Xml<'_>, tag: &BytesStart<'_>) -> Result<Option<u32>> {
    xml.attribute(tag, b"", b"numFmtId")?
        .map(|id| id.parse().map_err(xml_error))
        .transpose()
}

struct Row<'a> {
    number: u64,
    next_column: usize,
    cells: Vec<(usize, Cow<'a, str>)>,
}

impl Row<'_> {
    fn new(number: u64) -> Self {
        Self {
            number,
            next_column: 0,
            cells: Vec::new(),
        }
    }
}

struct Cell {
    column: usize,
    kind: Option<String>,
    style: Option<usize>,
    text: String,
    text_bytes: usize,
}

impl Cell {
    fn new(xml: &Xml<'_>, tag: &BytesStart<'_>, row: &Row<'_>) -> Result<Self> {
        let column = match xml.attribute(tag, b"", b"r")? {
            Some(reference) => {
                let (column, number) = cell_reference(&reference)?;
                if number != row.number {
                    return Err(invalid("cell reference does not match its row"));
                }
                column
            }
            None => row.next_column,
        };
        if column < row.next_column {
            return Err(invalid("row has duplicate or backwards cell references"));
        }
        Ok(Self {
            column,
            kind: xml.attribute(tag, b"", b"t")?,
            style: xml
                .attribute(tag, b"", b"s")?
                .map(|s| s.parse().map_err(xml_error))
                .transpose()?,
            text: String::new(),
            text_bytes: 0,
        })
    }

    fn append(&mut self, value: &str, budget: &mut Budget) -> Result<()> {
        let bytes = value.len() * 2;
        budget.retain(bytes)?;
        self.text_bytes += bytes;
        self.text.push_str(value);
        Ok(())
    }

    fn finish<'a>(
        self,
        row: &mut Row<'a>,
        shared: &'a [String],
        formats: &CellFormats,
        budget: &mut Budget,
    ) -> Result<usize> {
        row.next_column = self
            .column
            .checked_add(1)
            .ok_or_else(|| invalid("column overflows"))?;
        let raw = self.text.trim();
        if raw.is_empty() {
            budget.release(self.text_bytes);
            return Ok(0);
        }
        let (value, bytes) = match self.kind.as_deref() {
            Some("s") => {
                budget.visit()?;
                let index = raw.parse::<usize>().map_err(xml_error)?;
                let value = shared
                    .get(index)
                    .ok_or_else(|| invalid("cell references an absent shared string"))?;
                budget.expand(value.len())?;
                (Cow::Borrowed(value.as_str()), 0)
            }
            Some("inlineStr" | "str" | "e" | "d") => {
                budget.retain(raw.len())?;
                (Cow::Owned(raw.to_owned()), raw.len())
            }
            Some("b") => (Cow::Borrowed(if raw == "1" { "TRUE" } else { "FALSE" }), 0),
            _ => {
                let format = formats.code_for_style(self.style);
                let format_bytes = format.map_or(0, str::len);
                budget.expand(format_bytes)?;
                // f64 formatting may add a sign, punctuation, and up to 309
                // integral digits. Precision is bounded by the format string.
                let bound = (raw.len() + format_bytes + 320) * 2;
                budget.retain(bound)?;
                let value = format_cell(raw, format);
                let size = value.capacity();
                if size > bound {
                    return Err(invalid("numeric format exceeds its render bound"));
                }
                budget.release(bound - size);
                (Cow::Owned(value), size)
            }
        };
        budget.release(self.text_bytes);
        if value.trim().is_empty() {
            budget.release(bytes);
            return Ok(0);
        }
        let charge = bytes + 2 * size_of::<(usize, Cow<'_, str>)>() + 64;
        budget.retain(charge - bytes)?;
        row.cells.push((self.column, value));
        Ok(charge)
    }
}

fn cell_reference(reference: &str) -> Result<(usize, u64)> {
    let split = reference
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .count();
    let (letters, digits) = reference.split_at(split);
    let mut column = 0usize;
    for letter in letters.bytes() {
        column = column
            .checked_mul(26)
            .and_then(|value| {
                value.checked_add(usize::from(letter.to_ascii_uppercase() - b'A' + 1))
            })
            .ok_or_else(|| invalid("column reference overflows"))?;
    }
    let row = positive_row(digits)?;
    if column == 0 {
        return Err(invalid("cell reference has no column"));
    }
    Ok((column - 1, row))
}

fn positive_row(text: &str) -> Result<u64> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("invalid source row number"));
    }
    text.parse::<u64>()
        .ok()
        .filter(|&number| number != 0)
        .ok_or_else(|| invalid("invalid source row number"))
}

fn parse_sheet(
    bytes: &[u8],
    shared: &[String],
    formats: &CellFormats,
    budget: &mut Budget,
    through_row: &mut u64,
) -> Result<String> {
    let mut xml = Xml::new(bytes, Some(b"worksheet"))?;
    let mut out = String::new();
    let mut region = Region::default();
    let mut row = None;
    let mut cell = None;
    let mut text_depth = None;
    let mut inline = false;
    let mut run = false;
    let mut sheet_data = false;
    let mut saw_sheet_data = false;
    loop {
        let (depth, event) = xml.next()?;
        if let Some(text) = text_content(&event, depth, text_depth)? {
            if let Some(cell) = &mut cell {
                Cell::append(cell, &text, budget)?;
            }
            continue;
        }
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag)
                if depth == 1 && xml.sheet_element(tag.name(), b"sheetData") =>
            {
                if saw_sheet_data {
                    return Err(invalid("worksheet repeats sheetData"));
                }
                saw_sheet_data = true;
                sheet_data = !empty;
            }
            Event::End(_) if depth == 1 => sheet_data = false,
            Event::Start(tag) | Event::Empty(tag)
                if sheet_data && depth == 2 && xml.sheet_element(tag.name(), b"row") =>
            {
                budget.visit()?;
                let next = through_row
                    .checked_add(1)
                    .ok_or_else(|| invalid("row number overflows"))?;
                let number = match xml.attribute(&tag, b"", b"r")? {
                    Some(number) => positive_row(&number)?,
                    None => next,
                };
                if number < next {
                    return Err(invalid("worksheet has duplicate or backwards row numbers"));
                }
                if number > next || empty {
                    region.flush(&mut out, budget, through_row)?;
                    *through_row = number - 1;
                }
                if empty {
                    *through_row = number;
                } else {
                    row = Some(Row::new(number));
                }
            }
            Event::Start(tag) | Event::Empty(tag)
                if depth == 3 && row.is_some() && xml.sheet_element(tag.name(), b"c") =>
            {
                budget.visit()?;
                let row = row.as_mut().expect("matched row");
                let next = Cell::new(&xml, &tag, row)?;
                if empty {
                    region.bytes += next.finish(row, shared, formats, budget)?;
                } else {
                    cell = Some(next);
                }
            }
            Event::Start(tag) | Event::Empty(tag)
                if sheet_data
                    && (xml.sheet_element(tag.name(), b"row")
                        || xml.sheet_element(tag.name(), b"c")) =>
            {
                return Err(invalid("worksheet has a nested row or an out-of-row cell"));
            }
            Event::Start(tag) if cell.is_some() && depth == 4 => {
                inline = xml.sheet_element(tag.name(), b"is");
                if xml.sheet_element(tag.name(), b"v") {
                    text_depth = Some(depth + 1);
                }
            }
            Event::Start(tag) if inline && depth == 5 => {
                run = xml.sheet_element(tag.name(), b"r");
                if xml.sheet_element(tag.name(), b"t") {
                    text_depth = Some(depth + 1);
                }
            }
            Event::Start(tag) if run && depth == 6 && xml.sheet_element(tag.name(), b"t") => {
                text_depth = Some(depth + 1);
            }
            Event::End(_) => {
                if text_depth == Some(depth + 1) {
                    text_depth = None;
                }
                match depth {
                    5 => run = false,
                    4 => inline = false,
                    3 => {
                        if let (Some(cell), Some(row)) = (cell.take(), &mut row) {
                            region.bytes += cell.finish(row, shared, formats, budget)?;
                        }
                    }
                    2 => {
                        if let Some(row) = row.take() {
                            let number = row.number;
                            if row.cells.is_empty() {
                                region.flush(&mut out, budget, through_row)?;
                            } else {
                                region.push(row, *through_row, budget)?;
                            }
                            *through_row = number;
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => {
                if !saw_sheet_data {
                    return Err(invalid("worksheet has no sheetData"));
                }
                region.flush(&mut out, budget, through_row)?;
                return Ok(out);
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct Region<'a> {
    rows: Vec<Row<'a>>,
    bytes: usize,
    before: u64,
}

impl<'a> Region<'a> {
    fn push(&mut self, row: Row<'a>, before: u64, budget: &mut Budget) -> Result<()> {
        if self.rows.is_empty() {
            self.before = before;
        }
        let charge = 2 * size_of::<Row<'_>>();
        budget.retain(charge)?;
        self.bytes += charge;
        self.rows.push(row);
        Ok(())
    }

    fn flush(
        &mut self,
        out: &mut String,
        budget: &mut Budget,
        through_row: &mut u64,
    ) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        *through_row = self.before;
        // Rendering uses the union of populated columns of each table. No
        // vector is indexed by the largest referenced column.
        let header = self.rows.iter().position(|row| row.cells.len() >= 2);
        let (notes, table) = self.rows.split_at(header.unwrap_or(self.rows.len()));
        let mut columns = BTreeSet::new();
        for row in table {
            for &(column, _) in &row.cells {
                if !columns.contains(&column) {
                    budget.retain(64)?;
                    self.bytes += 64;
                    columns.insert(column);
                }
            }
        }
        for row in notes {
            separate(out, "\n\n", budget)?;
            push(out, row.cells[0].1.trim(), budget)?;
            *through_row = row.number;
        }
        if !table.is_empty() {
            separate(out, "\n\n", budget)?;
        }
        for (index, row) in table.iter().enumerate() {
            if index != 0 {
                push(out, "\n", budget)?;
            }
            push(out, "|", budget)?;
            let mut cells = row.cells.iter().peekable();
            for column in &columns {
                push(out, " ", budget)?;
                if let Some((_, value)) = cells.next_if(|(index, _)| index == column) {
                    escape_cell(out, value.trim(), budget)?;
                }
                push(out, " |", budget)?;
            }
            if index == 0 {
                push(out, "\n|", budget)?;
                for _ in &columns {
                    push(out, " --- |", budget)?;
                }
            }
            *through_row = row.number;
        }
        drop(columns);
        self.rows = Vec::new();
        budget.release(std::mem::take(&mut self.bytes));
        Ok(())
    }
}

fn separate(out: &mut String, separator: &str, budget: &mut Budget) -> Result<()> {
    if !out.is_empty() {
        push(out, separator, budget)?;
    }
    Ok(())
}

fn escape_cell(out: &mut String, value: &str, budget: &mut Budget) -> Result<()> {
    budget.check_unit((out.len() as u64).checked_add(value.len() as u64))?;
    let escapes = value.bytes().filter(|&byte| byte == b'|').count();
    admit_output(out, value.len() + escapes, budget)?;
    let mut parts = value.split('|');
    out.push_str(parts.next().unwrap_or_default());
    for part in parts {
        out.push_str("\\|");
        out.push_str(part);
    }
    Ok(())
}

fn push(out: &mut String, value: &str, budget: &mut Budget) -> Result<()> {
    admit_output(out, value.len(), budget)?;
    out.push_str(value);
    Ok(())
}

fn admit_output(out: &str, bytes: usize, budget: &mut Budget) -> Result<()> {
    let length = out.len().checked_add(bytes);
    budget.check_unit(length.map(|size| size as u64))?;
    budget.expand(bytes)?;
    // Account for amortized String growth and its old buffer during growth.
    budget.retain(bytes * 3)
}

/// ECMA-376 §18.8.30's implied `numFmt` codes. Ids at or above 164 are custom
/// and come from `styles.xml` instead; the ids omitted here are locale-defined
/// and have no fixed code, so they fall through to General.
fn builtin_number_format(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

/// Render a stored numeric value the way the sheet displays it.
///
/// This is the correctness-critical function: `0.5` under `0%` is **50%**, and
/// a date is a serial day count that would otherwise index as `45678`.
#[must_use]
pub fn format_cell(raw: &str, format_code: Option<&str>) -> String {
    let Ok(value) = raw.parse::<f64>() else {
        return raw.to_owned();
    };
    let Some(code) = format_code else {
        return trim_float(value);
    };
    let kind = classify_format(code);
    match kind {
        FormatKind::Percent => {
            let decimals = decimals_in(code);
            format!("{:.*}%", decimals, value * 100.0)
        }
        FormatKind::DateTime { time, date } => match serial_to_datetime(value) {
            Some((y, m, d, hh, mm, ss)) => match (date, time) {
                (true, false) => format!("{y:04}-{m:02}-{d:02}"),
                (false, true) => format!("{hh:02}:{mm:02}:{ss:02}"),
                _ => format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"),
            },
            None => trim_float(value),
        },
        FormatKind::Fixed => {
            let decimals = decimals_in(code);
            if decimals == 0 {
                trim_float(value.round())
            } else {
                format!("{:.*}", decimals, value)
            }
        }
        FormatKind::General => trim_float(value),
    }
}

enum FormatKind {
    General,
    Percent,
    Fixed,
    DateTime { date: bool, time: bool },
}

/// Classify a `numFmt` code. Literal text inside quotes and escaped characters
/// must not be read as format tokens — `"\"Q\"0"` is a literal `Q`, not a
/// quarter-of-year date token.
fn classify_format(code: &str) -> FormatKind {
    let mut in_quotes = false;
    let mut escaped = false;
    let mut in_bracket = false;
    let mut percent = false;
    let mut date = false;
    let mut time = false;
    let mut fixed = false;
    for ch in code.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => in_quotes = !in_quotes,
            // `[$-409]`, `[Red]`, `[h]` — locale/colour/elapsed sections.
            '[' if !in_quotes => in_bracket = true,
            ']' if !in_quotes => in_bracket = false,
            _ if in_quotes || in_bracket => {}
            '%' => percent = true,
            'y' | 'Y' | 'd' | 'D' => date = true,
            'h' | 'H' | 's' | 'S' => time = true,
            // `m` is minutes next to h/s and months otherwise; treating it as a
            // date token is right for `m/d/yy` and harmless for `h:mm`, which
            // already set `time`.
            'm' | 'M' => date = true,
            '0' | '#' | '?' => fixed = true,
            _ => {}
        }
    }
    if percent {
        return FormatKind::Percent;
    }
    if date || time {
        // `h:mm` alone sets both `date` (via m) and `time`; require an explicit
        // y/d to call it a date.
        let real_date =
            code.chars().any(|ch| matches!(ch, 'y' | 'Y' | 'd' | 'D')) || (date && !time);
        return FormatKind::DateTime {
            date: real_date,
            time,
        };
    }
    if fixed {
        return FormatKind::Fixed;
    }
    FormatKind::General
}

/// Digits after the decimal point the format asks for.
fn decimals_in(code: &str) -> usize {
    let mut best = 0;
    let mut counting = false;
    let mut count = 0;
    for ch in code.chars() {
        match ch {
            '.' => {
                counting = true;
                count = 0;
            }
            '0' | '#' | '?' if counting => count += 1,
            _ => {
                if counting {
                    best = best.max(count);
                    counting = false;
                }
            }
        }
    }
    if counting {
        best = best.max(count);
    }
    best
}

fn trim_float(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let mut text = format!("{value}");
    if text.contains('e') || text.contains('E') {
        text = format!("{value:.6}");
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

/// Excel's serial day count → civil date/time.
///
/// Day 1 is 1900-01-01, but the format also carries the deliberate 1900
/// leap-year bug: serial 60 is a nonexistent 1900-02-29. Serials at or below 60
/// are therefore not convertible to a real date and stay numeric rather than
/// being silently shifted by a day.
fn serial_to_datetime(serial: f64) -> Option<(i64, u32, u32, u32, u32, u32)> {
    if !serial.is_finite() || serial <= 60.0 || serial > 2_958_465.0 {
        return None;
    }
    let days = serial.trunc() as i64;
    let fraction = serial - serial.trunc();
    // Serial 25569 is 1970-01-01, which is `civil_from_days`' day 0. (Cross-
    // checks: serial 61 → 1900-03-01, serial 45672 → 2025-01-15.)
    const UNIX_EPOCH_SERIAL: i64 = 25_569;
    let (year, month, day) = civil_from_days(days - UNIX_EPOCH_SERIAL);
    let total_seconds = (fraction * 86_400.0).round() as u32;
    let (hour, minute, second) = (
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60,
    );
    Some((year, month, day, hour, minute, second))
}

/// Howard Hinnant's `civil_from_days`, with day 0 = 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use zip::{ZipWriter, write::SimpleFileOptions};

    use kio_core::error::Resource;
    use xml::{MAX_XML_ATTRIBUTES, MAX_XML_DEPTH};

    fn standard() -> Budget {
        Budget::new(ResourceTier::Standard)
    }

    fn sheet_xml(rows: &str) -> String {
        format!(
            r#"<worksheet xmlns="{}"><sheetData>{rows}</sheetData></worksheet>"#,
            std::str::from_utf8(xml::SPREADSHEET_NS).unwrap()
        )
    }

    fn render(rows: &str) -> String {
        parse_sheet(
            sheet_xml(rows).as_bytes(),
            &[],
            &CellFormats::default(),
            &mut standard(),
            &mut 0,
        )
        .unwrap()
    }

    #[test]
    fn metadata_memory_is_checked_before_growing_the_string_table() {
        let namespace = std::str::from_utf8(xml::SPREADSHEET_NS).unwrap();
        let input = format!(r#"<sst xmlns="{namespace}"><si/><si><t>x</t></si></sst>"#);
        let mut budget = standard();
        budget.memory_limit = (2 * RECORD_BYTES + 2) as u64;
        assert_eq!(
            parse_shared_strings(input.as_bytes(), &mut budget).unwrap(),
            ["", "x"]
        );
        let mut budget = standard();
        budget.memory_limit = (2 * RECORD_BYTES + 1) as u64;
        assert!(
            matches!(parse_shared_strings(input.as_bytes(), &mut budget),
            Err(AdapterError::ResourceLimit(limit)) if limit.resource == Resource::Memory)
        );
    }

    // ---- number formats --------------------------------------------------

    #[test]
    fn a_percent_format_turns_the_stored_fraction_into_what_the_sheet_shows() {
        // The live case from the dogfood corpus: 確認率 stores 0.5 and the
        // sheet displays 50%. Indexing `0.5` is a wrong answer, not a
        // formatting nicety.
        assert_eq!(format_cell("0.5", Some("0%")), "50%");
        assert_eq!(format_cell("0.5", Some("0.00%")), "50.00%");
        assert_eq!(format_cell("0.1234", Some("0.0%")), "12.3%");
    }

    #[test]
    fn a_date_serial_becomes_a_date_instead_of_a_day_count() {
        // Anchors, each independently checkable: 25569 is the Unix epoch,
        // 61 is the first serial past the 1900 leap-year bug, and 45672 is
        // 2025-01-15.
        assert_eq!(format_cell("25569", Some("yyyy-mm-dd")), "1970-01-01");
        assert_eq!(format_cell("61", Some("yyyy-mm-dd")), "1900-03-01");
        assert_eq!(format_cell("45672", Some("yyyy-mm-dd")), "2025-01-15");
        assert_eq!(format_cell("45672", Some("m/d/yy")), "2025-01-15");
    }

    #[test]
    fn a_serial_with_a_time_fraction_renders_both_halves() {
        // .5 of a day is noon.
        assert_eq!(
            format_cell("45672.5", Some("yyyy-mm-dd h:mm")),
            "2025-01-15 12:00:00"
        );
        assert_eq!(format_cell("45672.5", Some("h:mm:ss")), "12:00:00");
    }

    #[test]
    fn a_serial_inside_the_1900_leap_bug_is_not_silently_shifted() {
        // Serial 60 is Excel's nonexistent 1900-02-29. Rendering it as a real
        // date would be off by one; the honest answer is the raw number.
        assert_eq!(format_cell("60", Some("yyyy-mm-dd")), "60");
        assert_eq!(format_cell("1", Some("yyyy-mm-dd")), "1");
    }

    #[test]
    fn quoted_literals_are_not_read_as_format_tokens() {
        // `"Day "` contains `y` and `a`; only the tokens OUTSIDE the quotes
        // decide the format kind, so this is a fixed number, not a date.
        assert_eq!(format_cell("5", Some("\"Day \"0")), "5");
        assert_eq!(format_cell("1234.5", Some("#,##0.00")), "1234.50");
    }

    #[test]
    fn an_unformatted_number_keeps_its_own_precision() {
        assert_eq!(format_cell("42", None), "42");
        assert_eq!(format_cell("42.25", None), "42.25");
        assert_eq!(format_cell("not a number", Some("0%")), "not a number");
    }

    #[test]
    fn a_cell_reference_preserves_its_column_and_row() {
        assert_eq!(cell_reference("A1").unwrap(), (0, 1));
        assert_eq!(cell_reference("C2").unwrap(), (2, 2));
        assert_eq!(cell_reference("Z9").unwrap(), (25, 9));
        assert_eq!(cell_reference("AA1").unwrap(), (26, 1));
        assert!(cell_reference("A0").is_err());
        assert!(cell_reference("AA").is_err());
        assert!(cell_reference("ZZZZZZZZZZZZZZZZZZZZZZ1").is_err());
    }

    #[test]
    fn backwards_and_inconsistent_references_are_refused() {
        for rows in [
            r#"<row><c r="B1"/><c r="A1"/></row>"#,
            r#"<row r="4"><c r="B5"/></row>"#,
            r#"<row r="4"/><row r="2"/>"#,
        ] {
            assert!(
                parse_sheet(
                    sheet_xml(rows).as_bytes(),
                    &[],
                    &CellFormats::default(),
                    &mut standard(),
                    &mut 0
                )
                .is_err()
            );
        }
    }

    #[test]
    fn markdown_retains_tables_notes_and_escaped_values_in_sparse_columns() {
        let md = render(concat!(
            r#"<row><c t="str"><v>タイトル</v></c></row>"#,
            r#"<row><c t="str"><v>対象領域</v></c><c r="C2" t="str"><v>状態</v></c></row>"#,
            r#"<row><c t="str"><v>Kio|v1</v></c><c r="B3"/><c r="C3" t="str"><v>確認済</v></c></row>"#,
            r#"<row/><row><c t="str"><v>メモ</v></c></row>"#,
        ));
        assert_eq!(
            md,
            "タイトル\n\n| 対象領域 | 状態 |\n| --- | --- |\n| Kio\\|v1 | 確認済 |\n\nメモ"
        );
    }

    #[test]
    fn a_blank_row_starts_a_new_region_with_its_own_columns() {
        let md = render(concat!(
            r#"<row><c t="str"><v>確認率</v></c><c r="C1" t="str"><v>影響行</v></c></row>"#,
            r#"<row><c t="str"><v>50%</v></c><c r="C2"><v>74</v></c></row>"#,
            r#"<row/><row><c t="str"><v>領域</v></c><c t="str"><v>状態</v></c><c t="str"><v>担当</v></c></row>"#,
            r#"<row><c t="str"><v>決済</v></c><c t="str"><v>確認済</v></c><c t="str"><v>Platform</v></c></row>"#,
        ));
        assert_eq!(
            md,
            "| 確認率 | 影響行 |\n| --- | --- |\n| 50% | 74 |\n\n| 領域 | 状態 | 担当 |\n| --- | --- | --- |\n| 決済 | 確認済 | Platform |"
        );
    }

    #[test]
    fn p2_shared_references_and_escaped_output_are_charged() {
        let xml = sheet_xml(concat!(
            r#"<row r="99"><c r="A99" t="s"><v>0</v></c><c r="XFD99"><v>2</v></c></row>"#,
            r#"<row r="100"><c r="A100" t="s"><v>0</v></c></row>"#,
        ));
        let shared = ["a|b".to_owned()];
        let extract = |budget: &mut Budget, row: &mut u64| {
            parse_sheet(
                xml.as_bytes(),
                &shared,
                &CellFormats::default(),
                budget,
                row,
            )
        };
        let expected = "| a\\|b | 2 |\n| --- | --- |\n| a\\|b |  |";
        let mut exact = standard();
        exact.unit_limit = expected.len() as u64;
        let mut row = 0;
        assert_eq!(extract(&mut exact, &mut row).unwrap(), expected);
        assert_eq!(row, 100);
        let mut short = standard();
        short.unit_limit = expected.len() as u64 - 1;
        row = 0;
        assert!(
            matches!(extract(&mut short, &mut row), Err(AdapterError::ResourceLimit(limit))
            if limit.resource == Resource::UnitText && limit.observed == Some(expected.len() as u64))
        );
        assert_eq!(row, 99);

        let xml = sheet_xml(r#"<row><c t="s"><v>0</v></c><c t="s"><v>0</v></c></row>"#);
        let shared = ["x".repeat(1024)];
        let mut budget = standard();
        budget.work_limit = 6; // one row, two cells, two references and 2 KiB
        assert!(
            matches!(parse_sheet(xml.as_bytes(), &shared, &CellFormats::default(),
            &mut budget, &mut 0), Err(AdapterError::ResourceLimit(limit))
            if limit.resource == Resource::Work && limit.observed == Some(7))
        );
    }

    // ---- container -------------------------------------------------------

    #[test]
    fn a_non_zip_input_is_refused_rather_than_read_as_empty() {
        let error = extract_xlsx(b"not a zip at all").unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    // ---- end to end ------------------------------------------------------

    /// Build a CRC-valid ZIP with stored members. This exercises the same
    /// maintained reader used for production workbooks.
    fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in members {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .expect("start ZIP fixture member");
            writer.write_all(body).expect("write ZIP fixture member");
        }
        writer.finish().expect("finish ZIP fixture").into_inner()
    }

    /// Namespace-prefixed, like the writer this corpus was produced with —
    /// Excel itself writes the same elements unprefixed, and both must parse.
    const TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Default Extension="xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
    const ROOT: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    const WORKBOOK: &str = r#"<?xml version="1.0"?><x:workbook xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><x:sheets><x:sheet name="summary" sheetId="1" r:id="rId1"/></x:sheets></x:workbook>"#;
    const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    const SHARED: &str = r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>対象領域</t></si><si><t>確認率</t></si><si><t>決済 &amp; 照合</t></si></sst>"#;
    // Style 1 is `0%`; style 0 is General.
    const STYLES: &str = r#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts><numFmt numFmtId="164" formatCode="0%"/></numFmts><cellStyleXfs><xf numFmtId="0"/></cellStyleXfs><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs></styleSheet>"#;
    const SHEET: &str = r#"<?xml version="1.0"?><x:worksheet xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><x:sheetData><x:row r="1"><x:c r="A1" t="s"><x:v>0</x:v></x:c><x:c r="B1" t="s"><x:v>1</x:v></x:c></x:row><x:row r="2"><x:c r="A2" t="s"><x:v>2</x:v></x:c><x:c r="B2" s="1"><x:v>0.5</x:v></x:c></x:row></x:sheetData></x:worksheet>"#;

    fn workbook_fixture() -> Vec<u8> {
        zip_of(&[
            (MANIFEST, TYPES.as_bytes()),
            (ROOT_RELS, ROOT.as_bytes()),
            ("xl/workbook.xml", WORKBOOK.as_bytes()),
            ("xl/_rels/workbook.xml.rels", RELS.as_bytes()),
            ("xl/sharedStrings.xml", SHARED.as_bytes()),
            ("xl/styles.xml", STYLES.as_bytes()),
            ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
            ("xl/media/image1.png", b"\x89PNG-not-really"),
        ])
    }

    fn workbook_fixture_replacing(member: &str, replacement: &[u8]) -> Vec<u8> {
        let members = [
            (MANIFEST, TYPES.as_bytes()),
            (ROOT_RELS, ROOT.as_bytes()),
            ("xl/workbook.xml", WORKBOOK.as_bytes()),
            ("xl/_rels/workbook.xml.rels", RELS.as_bytes()),
            ("xl/sharedStrings.xml", SHARED.as_bytes()),
            ("xl/styles.xml", STYLES.as_bytes()),
            ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
        ];
        let members = members
            .into_iter()
            .map(|(name, body)| {
                if name == member {
                    (name, replacement)
                } else {
                    (name, body)
                }
            })
            .collect::<Vec<_>>();
        zip_of(&members)
    }

    #[derive(Debug)]
    struct Extracted {
        names: Vec<String>,
        sheets: Vec<String>,
        media_paths: Vec<String>,
    }

    fn extract_xlsx(input: &[u8]) -> Result<Extracted> {
        let mut workbook = XlsxWorkbook::open(input, ResourceTier::Standard)?;
        let names: Vec<_> = workbook.sheet_names().map(str::to_owned).collect();
        let media_paths = workbook.media_paths().map(str::to_owned).collect();
        let sheets = (0..names.len())
            .map(|index| workbook.read_sheet(index, &mut 0))
            .collect::<Result<_>>()?;
        Ok(Extracted {
            names,
            sheets,
            media_paths,
        })
    }

    #[test]
    fn p2_workbook_can_exceed_the_old_sheet_limit() {
        use std::fmt::Write as _;
        let mut workbook = WORKBOOK.split_once("<x:sheet ").unwrap().0.to_owned();
        let (rel_start, _) = RELS.split_once("<Relationship ").unwrap();
        let mut rels = rel_start.to_owned();
        let mut names = Vec::new();
        for number in 1..=257 {
            write!(
                workbook,
                r#"<x:sheet name="sheet {number}" sheetId="{number}" r:id="rId{number}"/>"#
            )
            .unwrap();
            write!(rels, r#"<Relationship Id="rId{number}" Type="{WORKSHEET_REL}" Target="worksheets/sheet{number}.xml"/>"#).unwrap();
            names.push(format!("xl/worksheets/sheet{number}.xml"));
        }
        workbook.push_str("</x:sheets></x:workbook>");
        rels.push_str("</Relationships>");
        let sheet = sheet_xml(r#"<row><c><v>1</v></c></row>"#);
        let mut members = vec![
            (MANIFEST, TYPES.as_bytes()),
            (ROOT_RELS, ROOT.as_bytes()),
            ("xl/workbook.xml", workbook.as_bytes()),
            ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ];
        members.extend(names.iter().map(|name| (name.as_str(), sheet.as_bytes())));
        let output = extract_xlsx(&zip_of(&members)).unwrap();
        assert_eq!(output.sheets.len(), 257);
        assert_eq!(output.names[256], "sheet 257");
        assert_eq!(output.sheets[256], "1");
    }

    #[test]
    fn p2_missing_package_authority_is_rejected() {
        let input = zip_of(&[
            ("xl/workbook.xml", WORKBOOK.as_bytes()),
            ("xl/_rels/workbook.xml.rels", RELS.as_bytes()),
            ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
            ("xl/sharedStrings.xml", SHARED.as_bytes()),
        ]);
        assert!(matches!(
            extract_xlsx(&input),
            Err(AdapterError::ContractViolation(_))
        ));
    }

    #[test]
    fn p2_sparse_rows_and_columns_exceed_the_old_limits() {
        let mut rows = String::new();
        for row in 1..=50_001 {
            use std::fmt::Write as _;
            write!(
                rows,
                "<row r='{row}'><c r='A{row}'><v>1</v></c><c r='XFD{row}'><v>2</v></c></row>"
            )
            .unwrap();
        }
        let input =
            workbook_fixture_replacing("xl/worksheets/sheet1.xml", sheet_xml(&rows).as_bytes());
        let output = extract_xlsx(&input).unwrap();
        assert_eq!(output.sheets[0].lines().count(), 50_002);
        assert!(output.sheets[0].ends_with("| 1 | 2 |"));
    }

    #[test]
    fn p2_package_authorities_must_match_through_eof() {
        for (part, content) in [
            (MANIFEST, TYPES.replace(WORKBOOK_CONTENT, "application/xml")),
            (
                ROOT_RELS,
                ROOT.replace("Target=", "TargetMode='External' Target="),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                RELS.replace(WORKSHEET_REL, STYLES_REL),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                RELS.replace("sheet1.xml", "missing.xml"),
            ),
            (
                "xl/worksheets/sheet1.xml",
                SHEET.replace("spreadsheetml/2006/main", "not-spreadsheetml"),
            ),
            ("xl/worksheets/sheet1.xml", format!("{SHEET}<another/>")),
        ] {
            assert!(
                matches!(
                    extract_xlsx(&workbook_fixture_replacing(part, content.as_bytes())),
                    Err(AdapterError::ContractViolation(_))
                ),
                "{part}: {content}"
            );
        }
    }

    #[test]
    fn p2_extracts_the_office_acceptance_fixture() {
        let input =
            include_bytes!("../../kio-eval/acceptance-fixtures/v1/office-public/table.xlsx");
        let output = extract_xlsx(input).unwrap();
        assert_eq!(output.names, ["Fixture"]);
        assert!(!output.sheets[0].is_empty());
    }

    #[test]
    fn a_workbook_extracts_to_a_markdown_table_with_formats_applied() {
        let document = extract_xlsx(&workbook_fixture()).expect("extract");
        assert_eq!(document.sheets.len(), 1);
        assert_eq!(document.names[0], "summary");
        assert_eq!(
            document.sheets[0],
            "| 対象領域 | 確認率 |\n| --- | --- |\n| 決済 & 照合 | 50% |"
        );
    }

    #[test]
    fn an_xml_entity_in_a_shared_string_is_decoded_once() {
        // `&amp;` must reach the index as `&` — the class of bug a hand-rolled
        // XML reader produces silently.
        let document = extract_xlsx(&workbook_fixture()).expect("extract");
        assert!(document.sheets[0].contains("決済 & 照合"));
        assert!(!document.sheets[0].contains("&amp;"));
    }

    #[test]
    fn embedded_media_is_reported_rather_than_dropped_silently() {
        // Direct extraction cannot read a chart. Counting it keeps the gap
        // visible until the image → OCR identity exists (backlog I9).
        let document = extract_xlsx(&workbook_fixture()).expect("extract");
        assert_eq!(document.media_paths, vec!["xl/media/image1.png"]);
    }

    #[test]
    fn a_workbook_with_no_sheets_is_refused() {
        let empty = workbook_fixture_replacing("xl/workbook.xml",
            br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheets/></workbook>"#);
        assert!(extract_xlsx(&empty).is_err());
    }

    #[test]
    fn malformed_xml_members_fail_before_partial_xlsx_extraction() {
        let malformed_members = [
            (
                "xl/workbook.xml",
                br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheets></workbook>"# as &[u8],
            ),
            (
                "xl/sharedStrings.xml",
                br#"<!DOCTYPE sst [<!ENTITY injected "not allowed">]><sst><si><t>&injected;</t></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:x="urn:same" xmlns:y="urn:same"><sheetData><row><c x:r="A1" y:r="B1"><v>1</v></c></row></sheetData></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c r="A1"><v>1</v></c>"#,
            ),
        ];
        for (member, malformed) in malformed_members {
            let error = extract_xlsx(&workbook_fixture_replacing(member, malformed))
                .expect_err("malformed XML member must reject the workbook");
            assert!(matches!(error, AdapterError::ContractViolation(_)));
        }
    }

    #[test]
    fn xml_structural_limits_reject_deep_or_attribute_dense_members() {
        let nested =
            sheet_xml(&("<level>".repeat(MAX_XML_DEPTH) + &"</level>".repeat(MAX_XML_DEPTH)));
        let error = extract_xlsx(&workbook_fixture_replacing(
            "xl/worksheets/sheet1.xml",
            nested.as_bytes(),
        ))
        .expect_err("over-deep XML member must reject the workbook");
        assert!(matches!(error, AdapterError::ContractViolation(_)));

        let attributes = (0..=MAX_XML_ATTRIBUTES)
            .map(|index| format!(" a{index}=\"value\""))
            .collect::<String>();
        let dense = sheet_xml(&format!("<row{attributes}/>"));
        let error = extract_xlsx(&workbook_fixture_replacing(
            "xl/worksheets/sheet1.xml",
            dense.as_bytes(),
        ))
        .expect_err("over-attribute XML member must reject the workbook");
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    #[test]
    fn media_matches_ooxml_only_and_never_the_legacy_binary() {
        assert!(is_xlsx_media(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        ));
        // `.xls` is OLE2, not ZIP. Accepting it would make an unsupported
        // format fail `kio index` outright, since an unreadable workbook is a
        // hard error by design here.
        assert!(!is_xlsx_media("application/vnd.ms-excel"));
        assert!(!is_xlsx_media("application/pdf"));
        assert!(!is_xlsx_media(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
    }
}
