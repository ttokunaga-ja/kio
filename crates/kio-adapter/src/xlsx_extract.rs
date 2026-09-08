//! Deterministic XLSX → Markdown extraction (07 §5.1, 2026-07-25 ruling).
//!
//! DOCX and PPTX unit-ize through a converted PDF because a page and a slide
//! ARE their visual units. A sheet is not: rendering one to PDF paginates it by
//! print area, which cuts a wide table down the middle and leaves the two
//! halves of every row in different units with nothing to rejoin them by. That
//! was measured on this corpus — a 10-column sheet became two pages, the
//! header `作業区分` landing on page 1 and its value on page 2.
//!
//! So XLSX is extracted **directly and locally**: the cells are already
//! structured text, and rendering them to pixels only to read them back is a
//! lossy round trip through information that was never lost. No provider call,
//! no cost, no batch lane, and a `prepared_hash` that is a pure function of the
//! bytes.
//!
//! # Number formats are not cosmetic
//!
//! A cell holding `0.5` under a `0%` format means **50%**, and every file in
//! the dogfood corpus declares custom `numFmt` entries. Emitting the raw stored
//! value would put `0.5` in the index where the document says `50%` — a
//! retrieval-poisoning wrong answer, not a formatting nicety. [`format_cell`]
//! applies the format; dates are the same problem (a date is stored as a serial
//! day count, so the raw value indexes as `45678`).
//!
//! # Container
//!
//! The ZIP layer is read here rather than through a crate: its failure mode is
//! a loud refusal, and the decompression bounds are the security-relevant part
//! (a small XLSX can inflate to gigabytes). The XML inside is parsed with
//! `quick-xml` — namespaced OOXML with entity escaping is where a hand-rolled
//! reader fails *silently*, and silent wrongness is the failure mode this
//! module must not have.

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::{AdapterError, Result};

/// Sheets extracted from one workbook. A workbook past this is a contract
/// violation rather than a truncation — a silently half-read spreadsheet is
/// exactly the "looks complete, is not" failure this crate keeps out.
pub const MAX_XLSX_SHEETS: usize = 256;

/// Rows read per sheet.
pub const MAX_XLSX_ROWS_PER_SHEET: usize = 50_000;

/// Columns read per row.
pub const MAX_XLSX_COLUMNS: usize = 1_024;

/// Cells materialized across a complete workbook, including the empty cells
/// introduced to preserve a sparse row's column positions.
pub const MAX_XLSX_CELLS_PER_WORKBOOK: usize = 1_000_000;

/// Text bytes materialized into cells across a complete workbook.
pub const MAX_XLSX_CELL_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// Markdown bytes rendered across a complete workbook.
pub const MAX_XLSX_MARKDOWN_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on any single inflated ZIP member. `xl/worksheets/sheetN.xml` is the
/// large one; 64 MB of sheet XML is far past any real document and stops a ZIP
/// bomb from being decompressed into memory.
pub const MAX_XLSX_MEMBER_BYTES: usize = 64 * 1024 * 1024;

/// Entries read from the ZIP central directory.
const MAX_ZIP_ENTRIES: usize = 4_096;

/// One worksheet, already rendered to Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XlsxSheet {
    /// Sheet name as authored, NFC-normalized by the caller's unit-key rule.
    pub name: String,
    /// Markdown table (plus any leading single-cell lines).
    pub markdown: String,
}

/// One workbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XlsxDocument {
    pub sheets: Vec<XlsxSheet>,
    /// `xl/media/*` entries found in the container.
    ///
    /// A chart or picture embedded in a sheet is genuinely visual and direct
    /// extraction cannot read it. Routing those to the image → OCR lane needs
    /// an evidence-pointer identity for "image N inside sheet M", which does
    /// not exist yet (`image_object_hashes` is declared on `PrepareStageOutput`
    /// and `PrepareResponse` but nothing populates or reads it). Counting them
    /// here keeps the gap VISIBLE instead of dropping the images silently —
    /// see tasks/step4b-backlog.md §7.2 I9.
    pub media_paths: Vec<String>,
}

/// Extract every worksheet of an XLSX as Markdown.
///
/// Errors are contract violations: a workbook we cannot read must not
/// degrade into an empty-but-successful extraction, which would index the file
/// as "present with no content".
pub fn extract_xlsx(bytes: &[u8]) -> Result<XlsxDocument> {
    let entries = read_zip_entries(bytes)?;
    let get = |name: &str| -> Option<&ZipEntry> { entries.iter().find(|entry| entry.name == name) };

    let workbook = get("xl/workbook.xml")
        .ok_or_else(|| AdapterError::ContractViolation("XLSX has no xl/workbook.xml".to_owned()))?;
    let workbook_xml = inflate_entry(bytes, workbook)?;
    let sheet_refs = parse_workbook_sheets(&workbook_xml)?;
    if sheet_refs.len() > MAX_XLSX_SHEETS {
        return Err(AdapterError::ContractViolation(format!(
            "XLSX declares {} sheets, over the {MAX_XLSX_SHEETS} bound",
            sheet_refs.len()
        )));
    }

    let rels = match get("xl/_rels/workbook.xml.rels") {
        Some(entry) => parse_relationships(&inflate_entry(bytes, entry)?)?,
        None => BTreeMap::new(),
    };
    let shared = match get("xl/sharedStrings.xml") {
        Some(entry) => parse_shared_strings(&inflate_entry(bytes, entry)?)?,
        None => Vec::new(),
    };
    let formats = match get("xl/styles.xml") {
        Some(entry) => parse_styles(&inflate_entry(bytes, entry)?)?,
        None => CellFormats::default(),
    };

    let mut sheets = Vec::with_capacity(sheet_refs.len());
    let mut materialized_cells = 0;
    let mut materialized_cell_text_bytes = 0;
    let mut rendered_markdown_bytes = 0;
    for (index, sheet_ref) in sheet_refs.iter().enumerate() {
        // Resolve through the relationship id when it is present; fall back to
        // positional `sheetN.xml`, which is what every writer emits anyway.
        let target = sheet_ref
            .rel_id
            .as_deref()
            .and_then(|id| rels.get(id))
            .map(|target| normalize_rel_target(target))
            .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", index + 1));
        let Some(entry) = get(&target) else {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX sheet `{}` points at missing part {target}",
                sheet_ref.name
            )));
        };
        let grid = parse_sheet(
            &inflate_entry(bytes, entry)?,
            &shared,
            &formats,
            &mut materialized_cells,
            &mut materialized_cell_text_bytes,
        )?;
        let markdown =
            grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES - rendered_markdown_bytes)?;
        rendered_markdown_bytes += markdown.len();
        sheets.push(XlsxSheet {
            name: sheet_ref.name.clone(),
            markdown,
        });
    }

    let media_paths = entries
        .iter()
        .filter(|entry| entry.name.starts_with("xl/media/"))
        .map(|entry| entry.name.clone())
        .collect();

    Ok(XlsxDocument {
        sheets,
        media_paths,
    })
}

/// True for the OOXML spreadsheet mime, and ONLY that one.
///
/// `application/vnd.ms-excel` (the legacy `.xls`) is deliberately excluded: it
/// is an OLE2 compound file, not a ZIP, so this extractor would refuse it — and
/// because an unreadable workbook is a hard error here (by design, so a file
/// cannot vanish silently), accepting `.xls` would turn "we don't support this
/// format" into a failed `kio index`. Nothing maps a `.xls` extension to that
/// mime today; this keeps it that way if something ever does.
#[must_use]
pub fn is_xlsx_media(media_type: &str) -> bool {
    media_type == "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
}

// ---------------------------------------------------------------------------
// ZIP container
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ZipEntry {
    name: String,
    /// Offset of the local file header.
    local_header_offset: usize,
    compression: u16,
    compressed_size: usize,
    uncompressed_size: usize,
}

/// Read the central directory. Only what an XLSX needs: stored and deflated
/// members, no encryption, no ZIP64 (a >4 GB spreadsheet is refused, loudly).
fn read_zip_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    const EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    const CD_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];

    if bytes.len() < 22 || !bytes.starts_with(b"PK") {
        return Err(AdapterError::ContractViolation(
            "XLSX is not a ZIP container".to_owned(),
        ));
    }
    // The EOCD is last, after a comment of up to 64 KiB. Scan backwards.
    let scan_start = bytes.len().saturating_sub(22 + 65_536);
    let eocd = (scan_start..=bytes.len() - 22)
        .rev()
        .find(|&offset| bytes[offset..offset + 4] == EOCD_SIGNATURE)
        .ok_or_else(|| {
            AdapterError::ContractViolation(
                "XLSX has no ZIP end-of-central-directory record".to_owned(),
            )
        })?;

    let entry_count = read_u16(bytes, eocd + 10)? as usize;
    let cd_size = read_u32(bytes, eocd + 12)? as usize;
    let cd_offset = read_u32(bytes, eocd + 16)? as usize;
    if entry_count > MAX_ZIP_ENTRIES {
        return Err(AdapterError::ContractViolation(format!(
            "XLSX declares {entry_count} ZIP entries, over the {MAX_ZIP_ENTRIES} bound"
        )));
    }
    if cd_offset.saturating_add(cd_size) > bytes.len() {
        return Err(AdapterError::ContractViolation(
            "XLSX central directory runs past the end of the file".to_owned(),
        ));
    }

    let mut entries = Vec::with_capacity(entry_count);
    let mut cursor = cd_offset;
    for _ in 0..entry_count {
        if cursor + 46 > bytes.len() || bytes[cursor..cursor + 4] != CD_SIGNATURE {
            return Err(AdapterError::ContractViolation(
                "XLSX central directory entry is malformed".to_owned(),
            ));
        }
        let compression = read_u16(bytes, cursor + 10)?;
        let compressed_size = read_u32(bytes, cursor + 20)? as usize;
        let uncompressed_size = read_u32(bytes, cursor + 24)? as usize;
        let name_len = read_u16(bytes, cursor + 28)? as usize;
        let extra_len = read_u16(bytes, cursor + 30)? as usize;
        let comment_len = read_u16(bytes, cursor + 32)? as usize;
        let local_header_offset = read_u32(bytes, cursor + 42)? as usize;
        let name_start = cursor + 46;
        let name_end = name_start + name_len;
        if name_end > bytes.len() {
            return Err(AdapterError::ContractViolation(
                "XLSX central directory name runs past the end of the file".to_owned(),
            ));
        }
        // OOXML part names are ASCII paths; anything else is not a part we read.
        let name = String::from_utf8_lossy(&bytes[name_start..name_end]).into_owned();
        entries.push(ZipEntry {
            name,
            local_header_offset,
            compression,
            compressed_size,
            uncompressed_size,
        });
        cursor = name_end + extra_len + comment_len;
    }
    Ok(entries)
}

fn inflate_entry(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>> {
    if entry.uncompressed_size > MAX_XLSX_MEMBER_BYTES {
        return Err(AdapterError::ContractViolation(format!(
            "XLSX part `{}` declares {} bytes, over the {MAX_XLSX_MEMBER_BYTES} bound",
            entry.name, entry.uncompressed_size
        )));
    }
    let header = entry.local_header_offset;
    if header + 30 > bytes.len() || bytes[header..header + 4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(AdapterError::ContractViolation(format!(
            "XLSX part `{}` has no local file header",
            entry.name
        )));
    }
    // The local header repeats the name/extra lengths, and they may differ from
    // the central directory's — the data starts after the LOCAL ones.
    let name_len = read_u16(bytes, header + 26)? as usize;
    let extra_len = read_u16(bytes, header + 28)? as usize;
    let data_start = header + 30 + name_len + extra_len;
    let data_end = data_start
        .checked_add(entry.compressed_size)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| {
            AdapterError::ContractViolation(format!(
                "XLSX part `{}` runs past the end of the file",
                entry.name
            ))
        })?;
    let data = &bytes[data_start..data_end];
    match entry.compression {
        0 => Ok(data.to_vec()),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, MAX_XLSX_MEMBER_BYTES)
            .map_err(|err| {
                AdapterError::ContractViolation(format!(
                    "XLSX part `{}` failed to inflate: {err:?}",
                    entry.name
                ))
            }),
        other => Err(AdapterError::ContractViolation(format!(
            "XLSX part `{}` uses unsupported ZIP compression method {other}",
            entry.name
        ))),
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|slice| u16::from_le_bytes([slice[0], slice[1]]))
        .ok_or_else(|| AdapterError::ContractViolation("XLSX ZIP structure truncated".to_owned()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|slice| u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
        .ok_or_else(|| AdapterError::ContractViolation("XLSX ZIP structure truncated".to_owned()))
}

/// `../media/x.png` and `worksheets/sheet1.xml` are both relative to `xl/`.
fn normalize_rel_target(target: &str) -> String {
    let trimmed = target.trim_start_matches('/');
    if let Some(rest) = trimmed.strip_prefix("../") {
        return rest.to_owned();
    }
    if trimmed.starts_with("xl/") {
        return trimmed.to_owned();
    }
    format!("xl/{trimmed}")
}

// ---------------------------------------------------------------------------
// OOXML
// ---------------------------------------------------------------------------

struct SheetRef {
    name: String,
    rel_id: Option<String>,
}

/// Local name of an element, with any namespace prefix dropped. Writers differ:
/// this corpus's files use `<x:sheet>` while Excel itself writes `<sheet>`.
fn local_name(raw: &[u8]) -> &[u8] {
    match raw.iter().position(|byte| *byte == b':') {
        Some(index) => &raw[index + 1..],
        None => raw,
    }
}

fn attribute(event: &quick_xml::events::BytesStart<'_>, wanted: &str) -> Option<String> {
    event.attributes().flatten().find_map(|attr| {
        (local_name(attr.key.as_ref()) == wanted.as_bytes())
            .then(|| String::from_utf8_lossy(&attr.value).into_owned())
    })
}

fn reader_for(xml: &[u8]) -> Reader<&[u8]> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_end_names = false;
    reader
}

fn xml_error(err: quick_xml::Error) -> AdapterError {
    AdapterError::ContractViolation(format!("XLSX XML is malformed: {err}"))
}

fn decoded_text(text: &quick_xml::events::BytesText<'_>) -> Result<String> {
    let decoded = text.decode().map_err(|error| {
        AdapterError::ContractViolation(format!("XLSX XML text is malformed: {error}"))
    })?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|error| {
            AdapterError::ContractViolation(format!("XLSX XML text is malformed: {error}"))
        })
}

/// quick-xml 0.41 surfaces references as their own event instead of folding
/// them into text. Resolve only XML's predefined/numeric entities; user-defined
/// entities have no OOXML-safe definition in this parser and must not become
/// silently missing text.
fn decoded_reference(reference: &quick_xml::events::BytesRef<'_>) -> Result<String> {
    if let Some(character) = reference.resolve_char_ref().map_err(|error| {
        AdapterError::ContractViolation(format!("XLSX XML text is malformed: {error}"))
    })? {
        return Ok(character.to_string());
    }
    let name = reference.decode().map_err(|error| {
        AdapterError::ContractViolation(format!("XLSX XML text is malformed: {error}"))
    })?;
    quick_xml::escape::resolve_predefined_entity(&name)
        .map(str::to_owned)
        .ok_or_else(|| {
            AdapterError::ContractViolation(format!(
                "XLSX XML contains unsupported entity reference `&{name};`"
            ))
        })
}

fn parse_workbook_sheets(xml: &[u8]) -> Result<Vec<SheetRef>> {
    let mut reader = reader_for(xml);
    let mut buf = Vec::new();
    let mut sheets = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(xml_error)? {
            Event::Empty(event) | Event::Start(event)
                if local_name(event.name().as_ref()) == b"sheet" =>
            {
                let Some(name) = attribute(&event, "name") else {
                    return Err(AdapterError::ContractViolation(
                        "XLSX workbook declares a sheet with no name".to_owned(),
                    ));
                };
                sheets.push(SheetRef {
                    name,
                    rel_id: attribute(&event, "id"),
                });
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if sheets.is_empty() {
        return Err(AdapterError::ContractViolation(
            "XLSX workbook declares no sheets".to_owned(),
        ));
    }
    Ok(sheets)
}

fn parse_relationships(xml: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut reader = reader_for(xml);
    let mut buf = Vec::new();
    let mut map = BTreeMap::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(xml_error)? {
            Event::Empty(event) | Event::Start(event)
                if local_name(event.name().as_ref()) == b"Relationship" =>
            {
                if let (Some(id), Some(target)) =
                    (attribute(&event, "Id"), attribute(&event, "Target"))
                {
                    map.insert(id, target);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(map)
}

/// `sharedStrings.xml` is an ordered table; a cell with `t="s"` holds an index.
/// Rich-text runs split one string across several `<t>` children, so the
/// concatenation is per `<si>`, not per `<t>`.
fn parse_shared_strings(xml: &[u8]) -> Result<Vec<String>> {
    let mut reader = reader_for(xml);
    let mut buf = Vec::new();
    let mut strings = Vec::new();
    let mut current: Option<String> = None;
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf).map_err(xml_error)? {
            Event::Start(event) => match local_name(event.name().as_ref()) {
                b"si" => current = Some(String::new()),
                b"t" => in_text = true,
                _ => {}
            },
            Event::End(event) => match local_name(event.name().as_ref()) {
                b"si" => strings.push(current.take().unwrap_or_default()),
                b"t" => in_text = false,
                _ => {}
            },
            Event::Text(text) if in_text => {
                if let Some(buffer) = current.as_mut() {
                    buffer.push_str(&decoded_text(&text)?);
                }
            }
            Event::GeneralRef(reference) if in_text => {
                if let Some(buffer) = current.as_mut() {
                    buffer.push_str(&decoded_reference(&reference)?);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(strings)
}

/// The `numFmt` code each `cellXfs` style index resolves to.
#[derive(Debug, Default)]
struct CellFormats {
    /// style index → numFmtId
    style_formats: Vec<u32>,
    /// numFmtId → format code, for the custom (≥164) ids
    custom: BTreeMap<u32, String>,
}

impl CellFormats {
    fn code_for_style(&self, style_index: Option<usize>) -> Option<&str> {
        let id = *self.style_formats.get(style_index?)?;
        if let Some(code) = self.custom.get(&id) {
            return Some(code.as_str());
        }
        builtin_number_format(id)
    }
}

fn parse_styles(xml: &[u8]) -> Result<CellFormats> {
    let mut reader = reader_for(xml);
    let mut buf = Vec::new();
    let mut formats = CellFormats::default();
    let mut in_cell_xfs = false;
    loop {
        match reader.read_event_into(&mut buf).map_err(xml_error)? {
            Event::Start(event) | Event::Empty(event) => {
                match local_name(event.name().as_ref()) {
                    b"numFmt" => {
                        if let (Some(id), Some(code)) = (
                            attribute(&event, "numFmtId").and_then(|v| v.parse::<u32>().ok()),
                            attribute(&event, "formatCode"),
                        ) {
                            formats.custom.insert(id, code);
                        }
                    }
                    b"cellXfs" => in_cell_xfs = true,
                    // `cellStyleXfs` also contains `<xf>` elements; only the
                    // `cellXfs` ones are what a cell's `s=` indexes into.
                    b"xf" if in_cell_xfs => {
                        formats.style_formats.push(
                            attribute(&event, "numFmtId")
                                .and_then(|v| v.parse::<u32>().ok())
                                .unwrap_or(0),
                        );
                    }
                    _ => {}
                }
            }
            Event::End(event) if local_name(event.name().as_ref()) == b"cellXfs" => {
                in_cell_xfs = false;
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(formats)
}

fn parse_sheet(
    xml: &[u8],
    shared: &[String],
    formats: &CellFormats,
    materialized_cells: &mut usize,
    materialized_cell_text_bytes: &mut usize,
) -> Result<Vec<Vec<String>>> {
    let mut reader = reader_for(xml);
    let mut buf = Vec::new();
    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_row = false;
    let mut cell: Option<CellState> = None;
    let mut text = String::new();
    let mut in_value = false;
    let mut in_inline_text = false;

    /// Place a finished cell at the column its `r` reference names, padding
    /// over the columns the writer omitted because they were empty. Without
    /// the padding a sparse row would shift left and a Markdown column would
    /// stop meaning one spreadsheet column.
    fn place(
        row: &mut Vec<String>,
        column: usize,
        value: String,
        materialized_cells: &mut usize,
    ) -> Result<()> {
        if column >= MAX_XLSX_COLUMNS {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX row references column {}, over the {MAX_XLSX_COLUMNS}-column bound",
                column + 1
            )));
        }
        if column < row.len() {
            return Err(AdapterError::ContractViolation(
                "XLSX row has duplicate or backwards cell references".to_owned(),
            ));
        }
        let additions = column + 1 - row.len();
        if materialized_cells.saturating_add(additions) > MAX_XLSX_CELLS_PER_WORKBOOK {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX workbook exceeds the {MAX_XLSX_CELLS_PER_WORKBOOK}-cell bound"
            )));
        }
        row.reserve(additions);
        row.resize_with(column, String::new);
        row.push(value);
        *materialized_cells += additions;
        Ok(())
    }

    fn admit_row(grid: &[Vec<String>], in_row: bool, cell: &Option<CellState>) -> Result<()> {
        if in_row || cell.is_some() {
            return Err(AdapterError::ContractViolation(
                "XLSX sheet has nested rows".to_owned(),
            ));
        }
        if grid.len() >= MAX_XLSX_ROWS_PER_SHEET {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX sheet exceeds the {MAX_XLSX_ROWS_PER_SHEET}-row bound"
            )));
        }
        Ok(())
    }

    loop {
        match reader.read_event_into(&mut buf).map_err(xml_error)? {
            Event::Start(event) => match local_name(event.name().as_ref()) {
                b"row" => {
                    admit_row(&grid, in_row, &cell)?;
                    in_row = true;
                    row = Vec::new();
                }
                b"c" => {
                    if !in_row || cell.is_some() {
                        return Err(AdapterError::ContractViolation(
                            "XLSX sheet has nested or out-of-row cells".to_owned(),
                        ));
                    }
                    text.clear();
                    cell = Some(CellState::from(&event, row.len())?);
                }
                b"v" => in_value = true,
                // `<is><t>` — an inline string, used by writers that skip the
                // shared table.
                b"t" => in_inline_text = true,
                _ => {}
            },
            // `<row/>` and `<c r="B2" s="4"/>` are self-closing: a styled but
            // empty cell is extremely common, and it never sees an `End`.
            Event::Empty(event) => match local_name(event.name().as_ref()) {
                b"row" => {
                    admit_row(&grid, in_row, &cell)?;
                    grid.push(Vec::new());
                }
                b"c" => {
                    if !in_row || cell.is_some() {
                        return Err(AdapterError::ContractViolation(
                            "XLSX sheet has nested or out-of-row cells".to_owned(),
                        ));
                    }
                    let state = CellState::from(&event, row.len())?;
                    place(&mut row, state.column, String::new(), materialized_cells)?;
                }
                _ => {}
            },
            Event::End(event) => match local_name(event.name().as_ref()) {
                b"row" => {
                    if !in_row || cell.is_some() {
                        return Err(AdapterError::ContractViolation(
                            "XLSX sheet has an unmatched row boundary".to_owned(),
                        ));
                    }
                    grid.push(std::mem::take(&mut row));
                    in_row = false;
                }
                b"c" => {
                    let state = cell.take().ok_or_else(|| {
                        AdapterError::ContractViolation(
                            "XLSX sheet has an unmatched cell boundary".to_owned(),
                        )
                    })?;
                    let value =
                        resolve_cell(&text, &state, shared, formats, materialized_cell_text_bytes)?;
                    place(&mut row, state.column, value, materialized_cells)?;
                    text.clear();
                }
                b"v" => in_value = false,
                b"t" => in_inline_text = false,
                _ => {}
            },
            Event::Text(content) if in_value || in_inline_text => {
                text.push_str(&decoded_text(&content)?);
            }
            Event::GeneralRef(reference) if in_value || in_inline_text => {
                text.push_str(&decoded_reference(&reference)?);
            }
            Event::Eof => {
                if in_row || cell.is_some() {
                    return Err(AdapterError::ContractViolation(
                        "XLSX sheet ends inside a row or cell".to_owned(),
                    ));
                }
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(grid)
}

struct CellState {
    column: usize,
    kind: Option<String>,
    style: Option<usize>,
}

impl CellState {
    fn from(event: &quick_xml::events::BytesStart<'_>, fallback_column: usize) -> Result<Self> {
        let column = match attribute(event, "r") {
            Some(reference) => column_index_from_ref_checked(&reference)?,
            None => fallback_column,
        };
        if column >= MAX_XLSX_COLUMNS {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX row references column {}, over the {MAX_XLSX_COLUMNS}-column bound",
                column + 1
            )));
        }
        Ok(Self {
            column,
            kind: attribute(event, "t"),
            style: attribute(event, "s").and_then(|value| value.parse::<usize>().ok()),
        })
    }
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

fn resolve_cell(
    raw: &str,
    state: &CellState,
    shared: &[String],
    formats: &CellFormats,
    materialized_cell_text_bytes: &mut usize,
) -> Result<String> {
    fn admit_text_bytes(used: &mut usize, bytes: usize) -> Result<()> {
        if used.saturating_add(bytes) > MAX_XLSX_CELL_TEXT_BYTES {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX workbook exceeds the {MAX_XLSX_CELL_TEXT_BYTES}-byte cell-text bound"
            )));
        }
        *used += bytes;
        Ok(())
    }

    fn formatted_value_bound(raw: &str, format_code: Option<&str>) -> usize {
        // A finite f64 needs at most 309 integral digits, a sign, decimal
        // separator, and the percent/date punctuation. The custom code bounds
        // the requested decimal places, so its byte length covers the only
        // unbounded part of the rendered numeric representation.
        raw.len()
            .saturating_add(format_code.map_or(0, str::len))
            .saturating_add(320)
    }

    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    match state.kind.as_deref() {
        Some("s") => {
            let index: usize = raw.parse().map_err(|_| {
                AdapterError::ContractViolation(format!(
                    "XLSX shared-string cell holds a non-numeric index `{raw}`"
                ))
            })?;
            let value = shared.get(index).ok_or_else(|| {
                AdapterError::ContractViolation(format!(
                    "XLSX cell references shared string {index}, past the {} in the table",
                    shared.len()
                ))
            })?;
            admit_text_bytes(materialized_cell_text_bytes, value.len())?;
            Ok(value.clone())
        }
        // Inline string, boolean, error, formula-string: already text.
        Some("inlineStr") | Some("str") | Some("e") => {
            admit_text_bytes(materialized_cell_text_bytes, raw.len())?;
            Ok(raw.to_owned())
        }
        Some("b") => {
            let value = if raw == "1" { "TRUE" } else { "FALSE" };
            admit_text_bytes(materialized_cell_text_bytes, value.len())?;
            Ok(value.to_owned())
        }
        // Numeric (`t` absent or `n`) — the branch where the stored value and
        // the document's meaning diverge.
        _ => {
            let format_code = formats.code_for_style(state.style);
            let reserved = formatted_value_bound(raw, format_code);
            admit_text_bytes(materialized_cell_text_bytes, reserved)?;
            let value = format_cell(raw, format_code);
            if value.len() > reserved {
                return Err(AdapterError::ContractViolation(
                    "XLSX numeric cell exceeds its checked render bound".to_owned(),
                ));
            }
            *materialized_cell_text_bytes -= reserved - value.len();
            Ok(value)
        }
    }
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

fn column_index_from_ref_checked(cell_ref: &str) -> Result<usize> {
    let mut index = 0usize;
    let mut saw_column = false;
    for ch in cell_ref.chars() {
        if !ch.is_ascii_alphabetic() {
            break;
        }
        saw_column = true;
        index = index
            .checked_mul(26)
            .and_then(|value| {
                value.checked_add(ch.to_ascii_uppercase() as usize - 'A' as usize + 1)
            })
            .ok_or_else(|| {
                AdapterError::ContractViolation("XLSX cell reference column overflows".to_owned())
            })?;
        if index > MAX_XLSX_COLUMNS {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX row references column {index}, over the {MAX_XLSX_COLUMNS}-column bound"
            )));
        }
    }
    if !saw_column {
        return Err(AdapterError::ContractViolation(
            "XLSX cell reference has no column".to_owned(),
        ));
    }
    Ok(index - 1)
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn populated(row: &[String]) -> usize {
    row.iter().filter(|cell| !cell.trim().is_empty()).count()
}

/// Render a sheet as Markdown.
///
/// A sheet is rarely one clean table. The corpus's files hold a title block, a
/// summary strip and the real table in a single sheet, separated by blank rows
/// — and the blocks do not share a column layout. Rendering the whole grid as
/// one table forces the widest block's columns onto every other one, so the
/// summary strip comes out as `| 対象項目 |  | 確認済 |  | …`: the header no
/// longer sits above its value.
///
/// So a blank row ends a region, and each region is rendered on its own terms:
/// its own header row and its own column trimming.
fn grid_to_markdown_with_limit(grid: &[Vec<String>], limit: usize) -> Result<String> {
    let mut out = BoundedMarkdown::new(limit);
    let mut has_block = false;
    for region in grid.split(|row| populated(row) == 0) {
        if region.is_empty() {
            continue;
        }
        if !region.iter().any(|row| populated(row) != 0) {
            continue;
        }
        if has_block {
            out.push_str("\n\n")?;
        }
        region_to_markdown(region, &mut out)?;
        out.trim_end();
        has_block = true;
    }
    out.trim_end();
    Ok(out.into_string())
}

struct BoundedMarkdown {
    out: String,
    limit: usize,
}

impl BoundedMarkdown {
    fn new(limit: usize) -> Self {
        Self {
            out: String::new(),
            limit,
        }
    }

    fn push_str(&mut self, value: &str) -> Result<()> {
        let length = self.out.len().checked_add(value.len()).ok_or_else(|| {
            AdapterError::ContractViolation("XLSX Markdown length overflows".to_owned())
        })?;
        if length > self.limit {
            return Err(AdapterError::ContractViolation(format!(
                "XLSX workbook Markdown exceeds the {MAX_XLSX_MARKDOWN_BYTES}-byte bound"
            )));
        }
        self.out.push_str(value);
        Ok(())
    }

    fn push_cell(&mut self, value: &str) -> Result<()> {
        let mut parts = value.split('|').peekable();
        while let Some(part) = parts.next() {
            self.push_str(part)?;
            if parts.peek().is_some() {
                self.push_str("\\|")?;
            }
        }
        Ok(())
    }

    fn trim_end(&mut self) {
        let length = self.out.trim_end().len();
        self.out.truncate(length);
    }

    fn into_string(self) -> String {
        self.out
    }
}

/// One blank-row-delimited region. A region whose rows never reach two
/// populated cells is prose (a title, a trailing note), not a table.
fn region_to_markdown(region: &[Vec<String>], out: &mut BoundedMarkdown) -> Result<()> {
    let width = region.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return Ok(());
    }
    let header_at = region.iter().position(|row| populated(row) >= 2);

    let preamble_end = header_at.unwrap_or(region.len());
    for row in &region[..preamble_end] {
        let mut has_cell = false;
        for cell in row
            .iter()
            .map(|cell| cell.trim())
            .filter(|cell| !cell.is_empty())
        {
            if has_cell {
                out.push_str(" ")?;
            }
            out.push_str(cell)?;
            has_cell = true;
        }
        if has_cell {
            out.push_str("\n\n")?;
        }
    }
    let Some(header_at) = header_at else {
        return Ok(());
    };

    let table = &region[header_at..];
    // Drop columns empty across this region: a writer emits a cell for every
    // column it has ever styled, and the spacer columns between a summary
    // strip's fields are exactly that.
    let keep: Vec<usize> = (0..width)
        .filter(|column| {
            table
                .iter()
                .any(|row| row.get(*column).is_some_and(|cell| !cell.trim().is_empty()))
        })
        .collect();
    if keep.is_empty() {
        return Ok(());
    }
    for (index, row) in table.iter().enumerate() {
        out.push_str("|")?;
        for column in &keep {
            out.push_str(" ")?;
            if let Some(cell) = row.get(*column) {
                out.push_cell(cell.trim())?;
            }
            out.push_str(" |")?;
        }
        out.push_str("\n")?;
        if index == 0 {
            out.push_str("|")?;
            for _ in &keep {
                out.push_str(" --- |")?;
            }
            out.push_str("\n")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ---- refs ------------------------------------------------------------

    #[test]
    fn a_cell_reference_resolves_to_its_column() {
        assert_eq!(column_index_from_ref_checked("A1").unwrap(), 0);
        assert_eq!(column_index_from_ref_checked("C2").unwrap(), 2);
        assert_eq!(column_index_from_ref_checked("Z9").unwrap(), 25);
        assert_eq!(column_index_from_ref_checked("AA1").unwrap(), 26);
    }

    #[test]
    fn self_closing_rows_and_cells_share_the_same_bounds_as_start_events() {
        let mut cells = 0;
        let mut text_bytes = 0;
        let self_closing_row = b"<worksheet><sheetData><row/></sheetData></worksheet>";
        let grid = parse_sheet(
            self_closing_row,
            &[],
            &CellFormats::default(),
            &mut cells,
            &mut text_bytes,
        )
        .expect("self-closing row");
        assert_eq!(grid, vec![Vec::<String>::new()]);

        let overflow = b"<worksheet><sheetData><row><c r=\"AMK1\"/></row></sheetData></worksheet>";
        assert!(
            parse_sheet(
                overflow,
                &[],
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes,
            )
            .is_err()
        );
    }

    #[test]
    fn self_closing_rows_and_cells_cannot_bypass_cardinality_limits() {
        let mut rows = String::from("<worksheet><sheetData>");
        for _ in 0..=MAX_XLSX_ROWS_PER_SHEET {
            rows.push_str("<row/>");
        }
        rows.push_str("</sheetData></worksheet>");
        let mut cells = 0;
        let mut text_bytes = 0;
        assert!(
            parse_sheet(
                rows.as_bytes(),
                &[],
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes,
            )
            .is_err()
        );

        let mut repeated_cells = String::from("<worksheet><sheetData><row>");
        for _ in 0..=MAX_XLSX_COLUMNS {
            repeated_cells.push_str("<c/>");
        }
        repeated_cells.push_str("</row></sheetData></worksheet>");
        let mut cells = 0;
        let mut text_bytes = 0;
        assert!(
            parse_sheet(
                repeated_cells.as_bytes(),
                &[],
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes,
            )
            .is_err()
        );
    }

    #[test]
    fn sparse_huge_and_backwards_references_are_refused_before_padding() {
        let mut cells = 0;
        let mut text_bytes = 0;
        let huge = b"<worksheet><sheetData><row><c r=\"ZZZ1\"/></row></sheetData></worksheet>";
        assert!(
            parse_sheet(
                huge,
                &[],
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes
            )
            .is_err()
        );

        let backwards =
            b"<worksheet><sheetData><row><c r=\"B1\"/><c r=\"A1\"/></row></sheetData></worksheet>";
        assert!(
            parse_sheet(
                backwards,
                &[],
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_start_and_empty_cells_preserve_sparse_columns() {
        let mut cells = 0;
        let mut text_bytes = 0;
        let xml = b"<worksheet><sheetData><row><c r=\"A1\"/><c r=\"C1\"><v>value</v></c></row><row/></sheetData></worksheet>";
        let grid = parse_sheet(
            xml,
            &[],
            &CellFormats::default(),
            &mut cells,
            &mut text_bytes,
        )
        .expect("parse");
        assert_eq!(
            grid[0],
            vec![String::new(), String::new(), "value".to_owned()]
        );
        assert_eq!(grid[1], Vec::<String>::new());
        assert_eq!(cells, 3);
    }

    #[test]
    fn workbook_cell_and_markdown_budgets_are_checked_before_growth() {
        let mut cells = MAX_XLSX_CELLS_PER_WORKBOOK - 1;
        let mut text_bytes = 0;
        let xml =
            b"<worksheet><sheetData><row><c r=\"A1\"/><c r=\"B1\"/></row></sheetData></worksheet>";
        assert!(
            parse_sheet(
                xml,
                &[],
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes
            )
            .is_err()
        );

        let mut cells = 0;
        let mut text_bytes = MAX_XLSX_CELL_TEXT_BYTES;
        let shared = vec!["x".repeat(1024)];
        let shared_cell =
            b"<worksheet><sheetData><row><c r=\"A1\" t=\"s\"><v>0</v></c></row></sheetData></worksheet>";
        assert!(
            parse_sheet(
                shared_cell,
                &shared,
                &CellFormats::default(),
                &mut cells,
                &mut text_bytes,
            )
            .is_err()
        );

        let grid = vec![vec!["a|b".to_owned(), "c".to_owned()]];
        assert!(grid_to_markdown_with_limit(&grid, 5).is_err());
    }

    // ---- markdown --------------------------------------------------------

    #[test]
    fn a_grid_renders_as_a_table_with_the_first_multi_cell_row_as_header() {
        let grid = vec![
            vec!["タイトル".into(), String::new()],
            vec!["対象領域".into(), "状態".into()],
            vec!["決済ルーティング".into(), "確認済".into()],
        ];
        let md = grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES).expect("render");
        assert_eq!(
            md,
            "タイトル\n\n| 対象領域 | 状態 |\n| --- | --- |\n| 決済ルーティング | 確認済 |"
        );
    }

    #[test]
    fn a_pipe_in_a_cell_cannot_break_the_table() {
        let grid = vec![vec!["a|b".into(), "c".into()], vec!["d".into(), "e".into()]];
        assert!(
            grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES)
                .expect("render")
                .contains("a\\|b")
        );
    }

    #[test]
    fn a_normal_sparse_table_still_renders_with_escaped_cells() {
        let grid = vec![
            vec!["title".into(), String::new(), "state".into()],
            vec!["Kio|v1".into(), String::new(), "ready".into()],
        ];
        assert_eq!(
            grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES).expect("render"),
            "| title | state |\n| --- | --- |\n| Kio\\|v1 | ready |"
        );
    }

    #[test]
    fn columns_empty_across_the_whole_sheet_are_dropped() {
        // Writers emit a cell for every styled column; a 10-column sheet
        // routinely carries several that hold nothing.
        let grid = vec![
            vec!["h1".into(), String::new(), "h2".into()],
            vec!["v1".into(), String::new(), "v2".into()],
        ];
        let md = grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES).expect("render");
        assert_eq!(md, "| h1 | h2 |\n| --- | --- |\n| v1 | v2 |");
    }

    #[test]
    fn a_blank_row_starts_a_new_region_with_its_own_columns() {
        // The corpus shape: a two-field summary strip with spacer columns,
        // then a blank row, then a three-column table. Rendered as ONE table
        // the strip's header would be pushed off its value by the wider
        // block's columns.
        let grid = vec![
            vec!["確認率".into(), String::new(), "影響行".into()],
            vec!["50%".into(), String::new(), "74".into()],
            vec![String::new(), String::new(), String::new()],
            vec!["領域".into(), "状態".into(), "担当".into()],
            vec!["決済".into(), "確認済".into(), "Platform".into()],
        ];
        let md = grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES).expect("render");
        assert_eq!(
            md,
            "| 確認率 | 影響行 |\n| --- | --- |\n| 50% | 74 |\n\n\
             | 領域 | 状態 | 担当 |\n| --- | --- | --- |\n| 決済 | 確認済 | Platform |"
        );
    }

    #[test]
    fn a_trailing_note_row_stays_prose_instead_of_becoming_a_one_cell_table() {
        let grid = vec![
            vec!["a".into(), "b".into()],
            vec![String::new(), String::new()],
            vec!["メモ：状態を更新してから統合判断へ進みます。".into()],
        ];
        let md = grid_to_markdown_with_limit(&grid, MAX_XLSX_MARKDOWN_BYTES).expect("render");
        assert!(md.ends_with("メモ：状態を更新してから統合判断へ進みます。"));
        assert!(!md.contains("| メモ"));
    }

    // ---- container -------------------------------------------------------

    #[test]
    fn a_non_zip_input_is_refused_rather_than_read_as_empty() {
        let error = extract_xlsx(b"not a zip at all").unwrap_err();
        assert!(matches!(error, AdapterError::ContractViolation(_)));
    }

    // ---- end to end ------------------------------------------------------

    /// Build a ZIP with stored (uncompressed) members. Enough for a workbook
    /// fixture, and it exercises the container reader for real rather than
    /// asserting against a hand-built parse tree.
    fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, body) in members {
            let offset = out.len() as u32;
            let crc = 0u32; // unchecked by the reader; the length fields are what it uses
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version..time/date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);

            directory.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            directory.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            directory.extend_from_slice(&crc.to_le_bytes());
            directory.extend_from_slice(&(body.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(body.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
            directory.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let cd_size = directory.len() as u32;
        out.extend_from_slice(&directory);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Namespace-prefixed, like the writer this corpus was produced with —
    /// Excel itself writes the same elements unprefixed, and both must parse.
    const WORKBOOK: &str = r#"<?xml version="1.0"?><x:workbook xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><x:sheets><x:sheet name="summary" sheetId="1" r:id="rId1"/></x:sheets></x:workbook>"#;
    const RELS: &str = r#"<?xml version="1.0"?><Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
    const SHARED: &str = r#"<?xml version="1.0"?><sst><si><t>対象領域</t></si><si><t>確認率</t></si><si><t>決済 &amp; 照合</t></si></sst>"#;
    // Style 1 is `0%`; style 0 is General.
    const STYLES: &str = r#"<?xml version="1.0"?><styleSheet><numFmts><numFmt numFmtId="164" formatCode="0%"/></numFmts><cellStyleXfs><xf numFmtId="0"/></cellStyleXfs><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs></styleSheet>"#;
    const SHEET: &str = r#"<?xml version="1.0"?><x:worksheet xmlns:x="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><x:sheetData><x:row r="1"><x:c r="A1" t="s"><x:v>0</x:v></x:c><x:c r="B1" t="s"><x:v>1</x:v></x:c></x:row><x:row r="2"><x:c r="A2" t="s"><x:v>2</x:v></x:c><x:c r="B2" s="1"><x:v>0.5</x:v></x:c></x:row></x:sheetData></x:worksheet>"#;

    fn workbook_fixture() -> Vec<u8> {
        zip_of(&[
            ("xl/workbook.xml", WORKBOOK.as_bytes()),
            ("xl/_rels/workbook.xml.rels", RELS.as_bytes()),
            ("xl/sharedStrings.xml", SHARED.as_bytes()),
            ("xl/styles.xml", STYLES.as_bytes()),
            ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
            ("xl/media/image1.png", b"\x89PNG-not-really"),
        ])
    }

    #[test]
    fn a_workbook_extracts_to_a_markdown_table_with_formats_applied() {
        let document = extract_xlsx(&workbook_fixture()).expect("extract");
        assert_eq!(document.sheets.len(), 1);
        assert_eq!(document.sheets[0].name, "summary");
        assert_eq!(
            document.sheets[0].markdown,
            "| 対象領域 | 確認率 |\n| --- | --- |\n| 決済 & 照合 | 50% |"
        );
    }

    #[test]
    fn an_xml_entity_in_a_shared_string_is_decoded_once() {
        // `&amp;` must reach the index as `&` — the class of bug a hand-rolled
        // XML reader produces silently.
        let document = extract_xlsx(&workbook_fixture()).expect("extract");
        assert!(document.sheets[0].markdown.contains("決済 & 照合"));
        assert!(!document.sheets[0].markdown.contains("&amp;"));
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
        let empty = zip_of(&[(
            "xl/workbook.xml",
            br#"<?xml version="1.0"?><workbook><sheets/></workbook>"# as &[u8],
        )]);
        assert!(extract_xlsx(&empty).is_err());
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
