//! Local XLSX extraction with one worksheet per prepared unit.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::{BufRead, BufReader},
};

use kio_core::error::ResourceTier;
use quick_xml::events::{BytesStart, Event};

use crate::{
    AdapterError, Result,
    ooxml_package::{BoundedZip, BoundedZipLimits, xml as opc},
};

mod budget;
mod values;
mod xml;

use values::{Lookup, Values, ValuesWriter};

use budget::Budget;
pub use budget::SheetUsage;
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
    shared: Values<String>,
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
            Xml::validate(&mut &*bytes, budget)?;
            opc::office_document_target(bytes, |size| retain_record(budget, size))
        })?;
        let content = with_part(&mut archive, &mut budget, MANIFEST, |bytes, budget| {
            Xml::validate(&mut &*bytes, budget)?;
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
        let pending = with_xml_member(&mut archive, &mut budget, &main, parse_workbook_sheets)?;
        let relations_part = relationship_part(&main);
        let mut worksheets = HashMap::new();
        let mut shared_part = None;
        let mut styles_part = None;
        with_part(
            &mut archive,
            &mut budget,
            &relations_part,
            |bytes, budget| {
                Xml::validate(&mut &*bytes, budget)?;
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
                with_xml_member(&mut archive, &mut budget, &name, |input, budget| {
                    Xml::validate(input, budget)
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
            Some(part) => with_xml_member(&mut archive, &mut budget, &part, parse_shared_strings)?,
            None => Values::default(),
        };
        let formats = match styles_part {
            Some(part) => with_xml_member(&mut archive, &mut budget, &part, parse_styles)?,
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
    pub fn read_sheet(
        &mut self,
        index: usize,
        progress: &mut SheetProgress,
    ) -> Result<(String, SheetUsage)> {
        *progress = SheetProgress::default();
        let sheet = self
            .sheets
            .get(index)
            .ok_or_else(|| invalid("worksheet index is out of range"))?;
        let before = self.budget.retained();
        let checkpoint = self.budget.begin_sheet();
        let result = with_xml_member(
            &mut self.archive,
            &mut self.budget,
            &sheet.target,
            |input, budget| {
                parse_sheet_stream(input, &self.shared, &self.formats, budget, progress)
            },
        );
        self.budget.release(self.budget.retained() - before);
        result.map(|markdown| {
            let usage = self.budget.sheet_usage(checkpoint, markdown.len());
            (markdown, usage)
        })
    }

    pub fn reuse_sheet(&mut self, usage: SheetUsage) -> Result<()> {
        self.budget.replay(usage)
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
    budget.set_xml_scratch(0)?;
    result
}

fn with_xml_member<T>(
    archive: &mut BoundedZip<'_>,
    budget: &mut Budget,
    name: &str,
    parse: impl FnOnce(&mut dyn BufRead, &mut Budget) -> Result<T>,
) -> Result<T> {
    const BUFFER: usize = 16 * 1024;
    budget.retain(BUFFER)?;
    let result = archive.read_member(name, |input, length| {
        budget.visit()?;
        budget.expand(length)?;
        parse(&mut BufReader::with_capacity(BUFFER, input), budget)
    });
    budget.release(BUFFER);
    budget.set_xml_scratch(0)?;
    result?.ok_or_else(|| invalid("required XML part is missing"))
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct SheetProgress {
    pub parsed_through_row: u64,
    pub completed_through_row: u64,
    pub first_unprocessed_row: Option<u64>,
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

fn parse_workbook_sheets(
    input: &mut dyn BufRead,
    budget: &mut Budget,
) -> Result<Vec<(String, String)>> {
    let mut xml = Xml::new(input, Some(b"workbook"));
    let mut buffer = Vec::new();
    let mut sheets = Vec::new();
    let mut in_sheets = false;
    loop {
        match xml.next(&mut buffer, budget)? {
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

fn parse_shared_strings(input: &mut dyn BufRead, budget: &mut Budget) -> Result<Values<String>> {
    let mut xml = Xml::new(input, Some(b"sst"));
    let mut buffer = Vec::new();
    let mut strings = ValuesWriter::default();
    let mut current = String::new();
    let mut current_bytes = 0;
    let mut in_item = false;
    let mut run = false;
    let mut text_depth = None;
    loop {
        let (depth, event) = xml.next(&mut buffer, budget)?;
        if let Some(text) = text_content(&event, depth, text_depth)? {
            budget.retain(text.len() * 2)?;
            current_bytes += text.len() * 2;
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
                    strings.push(String::new(), RECORD_BYTES, budget)?;
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
                strings.push(
                    std::mem::take(&mut current),
                    RECORD_BYTES + std::mem::take(&mut current_bytes),
                    budget,
                )?;
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
            Event::Eof => return strings.finish(budget),
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

fn parse_styles(input: &mut dyn BufRead, budget: &mut Budget) -> Result<CellFormats> {
    let mut xml = Xml::new(input, Some(b"styleSheet"));
    let mut buffer = Vec::new();
    let mut formats = CellFormats::default();
    let mut in_custom = false;
    let mut in_cell_xfs = false;
    loop {
        let (depth, event) = xml.next(&mut buffer, budget)?;
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

#[derive(serde::Serialize, serde::Deserialize)]
struct Row {
    number: u64,
    #[serde(skip)]
    next_column: usize,
    #[serde(skip)]
    bytes: usize,
    cells: Vec<(usize, CellValue)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum CellValue {
    Text(String),
    Shared(usize),
}

impl Row {
    fn new(number: u64) -> Self {
        Self {
            number,
            next_column: 0,
            bytes: 0,
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
    fn new(xml: &Xml<'_>, tag: &BytesStart<'_>, row: &Row) -> Result<Self> {
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

    fn finish(
        self,
        row: &mut Row,
        shared: &mut Lookup<'_, String>,
        formats: &CellFormats,
        budget: &mut Budget,
    ) -> Result<()> {
        row.next_column = self
            .column
            .checked_add(1)
            .ok_or_else(|| invalid("column overflows"))?;
        let raw = self.text.trim();
        if raw.is_empty() {
            budget.release(self.text_bytes);
            return Ok(());
        }
        if self
            .style
            .is_some_and(|index| index >= formats.style_formats.len())
        {
            return Err(invalid("cell references an absent style"));
        }
        let (value, bytes) = match self.kind.as_deref() {
            Some("s") => {
                budget.visit()?;
                let index = raw.parse::<usize>().map_err(xml_error)?;
                let empty = shared.get(index, budget, |value, budget| {
                    budget.expand(value.len())?;
                    Ok(value.trim().is_empty())
                })?;
                if empty {
                    budget.release(self.text_bytes);
                    return Ok(());
                }
                (CellValue::Shared(index), 0)
            }
            Some("inlineStr" | "str" | "e" | "d") => {
                budget.retain(raw.len())?;
                (CellValue::Text(raw.to_owned()), raw.len())
            }
            Some("b") => {
                let value = if raw == "1" { "TRUE" } else { "FALSE" };
                budget.retain(value.len())?;
                (CellValue::Text(value.to_owned()), value.len())
            }
            _ => {
                let format = formats.code_for_style(self.style);
                let format_bytes = format.map_or(0, str::len);
                budget.expand(format_bytes)?;
                let bound = (raw.len() + format_bytes + 320) * 2;
                budget.retain(bound)?;
                let value = format_cell(raw, format);
                let size = value.capacity();
                if size > bound {
                    return Err(invalid("numeric format exceeds its render bound"));
                }
                budget.release(bound - size);
                (CellValue::Text(value), size)
            }
        };
        budget.release(self.text_bytes);
        let slot = 2 * size_of::<(usize, CellValue)>() + 64;
        budget.retain(slot)?;
        row.bytes += bytes + slot;
        row.cells.push((self.column, value));
        Ok(())
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

fn parse_sheet_stream(
    input: &mut dyn BufRead,
    shared: &Values<String>,
    formats: &CellFormats,
    budget: &mut Budget,
    progress: &mut SheetProgress,
) -> Result<String> {
    let mut xml = Xml::new(input, Some(b"worksheet"));
    let mut shared = shared.lookup();
    let mut buffer = Vec::new();
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
        let (depth, event) = xml.next(&mut buffer, budget)?;
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
                let next = progress
                    .parsed_through_row
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
                    region.flush(&mut out, &mut shared, budget, progress)?;
                }
                progress.first_unprocessed_row.get_or_insert(number);
                budget.visit()?;
                if empty {
                    progress.parsed_through_row = number;
                    progress.completed_through_row = number;
                    progress.first_unprocessed_row = None;
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
                    next.finish(row, &mut shared, formats, budget)?;
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
                            cell.finish(row, &mut shared, formats, budget)?;
                        }
                    }
                    2 => {
                        if let Some(row) = row.take() {
                            let number = row.number;
                            progress.parsed_through_row = number;
                            if row.cells.is_empty() {
                                region.flush(&mut out, &mut shared, budget, progress)?;
                                progress.completed_through_row = number;
                                progress.first_unprocessed_row = None;
                            } else {
                                region.push(row, budget)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => {
                if !saw_sheet_data {
                    return Err(invalid("worksheet has no sheetData"));
                }
                region.flush(&mut out, &mut shared, budget, progress)?;
                return Ok(out);
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct Region {
    rows: ValuesWriter<Row>,
    columns: BTreeSet<usize>,
    last_row: u64,
}

impl Region {
    fn push(&mut self, row: Row, budget: &mut Budget) -> Result<()> {
        if row.cells.len() >= 2 || !self.columns.is_empty() {
            for &(column, _) in &row.cells {
                if !self.columns.contains(&column) {
                    budget.retain(64)?;
                    self.columns.insert(column);
                }
            }
        }
        budget.retain(2 * size_of::<Row>())?;
        let charge = row.bytes + 2 * size_of::<Row>();
        self.last_row = row.number;
        self.rows.push(row, charge, budget)
    }

    fn flush(
        &mut self,
        out: &mut String,
        shared: &mut Lookup<'_, String>,
        budget: &mut Budget,
        progress: &mut SheetProgress,
    ) -> Result<()> {
        let rows = std::mem::take(&mut self.rows).finish(budget)?;
        let mut in_table = false;
        rows.for_each(budget, |row, budget| {
            progress.first_unprocessed_row = Some(row.number);
            if !in_table && row.cells.len() == 1 {
                separate(out, "\n\n", budget)?;
                write_value(&row.cells[0].1, shared, budget, |value, budget| {
                    push(out, value, budget)
                })?;
            } else {
                let header = !in_table;
                separate(out, if header { "\n\n" } else { "\n" }, budget)?;
                push(out, "|", budget)?;
                let mut cells = row.cells.iter().peekable();
                for column in &self.columns {
                    push(out, " ", budget)?;
                    if let Some((_, value)) = cells.next_if(|(index, _)| index == column) {
                        write_value(value, shared, budget, |value, budget| {
                            escape_cell(out, value, budget)
                        })?;
                    }
                    push(out, " |", budget)?;
                }
                if header {
                    push(out, "\n|", budget)?;
                    for _ in &self.columns {
                        push(out, " --- |", budget)?;
                    }
                }
                in_table = true;
            }
            progress.completed_through_row = row.number;
            // Explicit blanks and coordinate gaps terminate regions.
            progress.first_unprocessed_row = (row.number < self.last_row).then(|| row.number + 1);
            Ok(())
        })?;
        let bytes = rows.resident() + self.columns.len() * 64;
        self.columns.clear();
        self.last_row = 0;
        drop(rows);
        budget.release(bytes);
        Ok(())
    }
}

fn write_value(
    value: &CellValue,
    shared: &mut Lookup<'_, String>,
    budget: &mut Budget,
    write: impl FnOnce(&str, &mut Budget) -> Result<()>,
) -> Result<()> {
    match value {
        CellValue::Text(value) => write(value.trim(), budget),
        CellValue::Shared(index) => {
            shared.get(*index, budget, |value, budget| write(value.trim(), budget))
        }
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

    fn parse_sheet(
        bytes: &[u8],
        shared: &[String],
        formats: &CellFormats,
        budget: &mut Budget,
        row: &mut u64,
    ) -> Result<String> {
        let mut writer = ValuesWriter::default();
        for value in shared {
            writer.push(value.clone(), 0, budget)?;
        }
        let shared = writer.finish(budget)?;
        let mut progress = SheetProgress::default();
        let value = parse_sheet_stream(&mut &*bytes, &shared, formats, budget, &mut progress);
        *row = progress.completed_through_row;
        value
    }

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
        let before = budget.begin_sheet();
        let values = parse_shared_strings(&mut input.as_bytes(), &mut budget).unwrap();
        let mut output = Vec::new();
        values
            .for_each(&mut budget, |value, _| {
                output.push(value.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(output, ["", "x"]);
        let required = budget.sheet_usage(before, 0).peak_bytes();
        let mut budget = standard();
        budget.memory_limit = required - 1;
        assert!(
            matches!(parse_shared_strings(&mut input.as_bytes(), &mut budget),
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
    fn review_rejects_invalid_xml_names_and_explicit_missing_styles() {
        for rows in ["<7invalid/>", "<row><c s='7'><v>1</v></c></row>"] {
            let input = sheet_xml(rows);
            assert!(
                matches!(
                    parse_sheet(
                        input.as_bytes(),
                        &[],
                        &CellFormats::default(),
                        &mut standard(),
                        &mut 0
                    ),
                    Err(AdapterError::ContractViolation(_))
                ),
                "{rows}"
            );
        }
    }

    #[test]
    fn review_reports_only_rendered_rows_when_parsing_stops() {
        let input = sheet_xml("<row r='50'/><row r='99'><c><v>2</v></c></row>");
        let mut budget = standard();
        budget.work_limit = 5;
        let mut progress = SheetProgress::default();
        assert!(matches!(
            parse_sheet_stream(
                &mut input.as_bytes(),
                &Values::default(),
                &CellFormats::default(),
                &mut budget,
                &mut progress
            ),
            Err(AdapterError::ResourceLimit(_))
        ));
        assert_eq!(progress.completed_through_row, 50);
        assert_eq!(progress.parsed_through_row, 50);
        assert_eq!(progress.first_unprocessed_row, Some(99));

        let input = sheet_xml("<row r='50'><c><v>1</v></c></row><row r='51'><c><v>2</v></c></row>");
        let mut budget = standard();
        budget.work_limit = 6;
        progress = SheetProgress::default();
        assert!(matches!(
            parse_sheet_stream(
                &mut input.as_bytes(),
                &Values::default(),
                &CellFormats::default(),
                &mut budget,
                &mut progress
            ),
            Err(AdapterError::ResourceLimit(_))
        ));
        assert_eq!(progress.completed_through_row, 0);
        assert_eq!(progress.parsed_through_row, 50);
        assert_eq!(progress.first_unprocessed_row, Some(50));
    }

    #[test]
    fn review_renders_a_long_region_with_bounded_memory() {
        let input = sheet_xml(&"<row><c><v>1</v></c><c><v>2</v></c></row>".repeat(20_000));
        let mut budget = standard();
        budget.memory_limit = 3 * 1024 * 1024;
        let markdown = parse_sheet(
            input.as_bytes(),
            &[],
            &CellFormats::default(),
            &mut budget,
            &mut 0,
        )
        .unwrap();
        assert_eq!(markdown.lines().count(), 20_001);
    }

    #[test]
    fn review_shared_strings_spill_under_the_same_memory_budget() {
        use std::fmt::Write as _;
        let namespace = std::str::from_utf8(xml::SPREADSHEET_NS).unwrap();
        let mut xml = format!("<sst xmlns='{namespace}'>");
        let padding = "v".repeat(256);
        for index in 0..6_000 {
            write!(xml, "<si><t>{index}:{padding}</t></si>").unwrap();
        }
        xml.push_str("</sst>");
        let mut budget = standard();
        budget.memory_limit = 1024 * 1024;
        let shared = parse_shared_strings(&mut xml.as_bytes(), &mut budget).unwrap();
        budget.set_xml_scratch(0).unwrap();
        assert!(shared.resident() < 128 * 1024);
        let mut shared = shared.lookup();
        for index in [5999, 0, 900, 901, 5999] {
            shared
                .get(index, &mut budget, |value, _| {
                    assert_eq!(value, &format!("{index}:{padding}"));
                    Ok(())
                })
                .unwrap();
        }
    }

    #[test]
    fn review_repeated_namespace_attributes_charge_work_without_multiplying_memory() {
        use std::fmt::Write as _;
        let mut input = format!("<ignored xmlns:a='urn:{}'", "x".repeat(8192));
        for index in 0..512 {
            write!(input, " a:name{index}='1'").unwrap();
        }
        input.push_str("/>");
        let mut budget = standard();
        budget.memory_limit = 1024 * 1024;
        Xml::validate(&mut input.as_bytes(), &mut budget).unwrap();

        let mut budget = standard();
        budget.memory_limit = 1024 * 1024;
        budget.work_limit = 700;
        assert!(matches!(Xml::validate(&mut input.as_bytes(), &mut budget),
            Err(AdapterError::ResourceLimit(limit)) if limit.resource == Resource::Work));
    }

    #[test]
    fn review_charges_xml_attribute_work() {
        let input = sheet_xml("<ignored xmlns:a='urn:long' a:x='1' a:y='2' a:z='3'/>");
        let mut budget = standard();
        budget.work_limit = 2;
        assert!(matches!(
            parse_sheet(
                input.as_bytes(),
                &[],
                &CellFormats::default(),
                &mut budget,
                &mut 0
            ),
            Err(AdapterError::ResourceLimit(_))
        ));
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
        budget.work_limit = 9; // The row, cells, references, attributes and expanded bytes.
        assert!(
            matches!(parse_sheet(xml.as_bytes(), &shared, &CellFormats::default(),
            &mut budget, &mut 0), Err(AdapterError::ResourceLimit(limit))
            if limit.resource == Resource::Work && limit.observed == Some(10))
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
            .map(|index| {
                workbook
                    .read_sheet(index, &mut SheetProgress::default())
                    .map(|(markdown, _)| markdown)
            })
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
