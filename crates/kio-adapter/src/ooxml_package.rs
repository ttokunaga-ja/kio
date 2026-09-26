//! Bounded OOXML ZIP package reader shared by spreadsheet extraction and the
//! real Office converter. The raw envelope is checked before the maintained
//! ZIP reader constructs its central-directory index; package parts are only
//! materialized through CRC-verifying, bounded reads.

use std::{
    collections::HashSet,
    io::{Cursor, Read},
};

use zip::{
    CompressionMethod, ZipArchive,
    read::{ArchiveOffset, Config},
};

use crate::{AdapterError, Result};

mod xml;

const MAX_ZIP_ENTRIES: usize = 4_096;
const MAX_ZIP_DECLARED_BYTES: usize = 256 * 1024 * 1024;
const EOCD_LEN: usize = 22;
const MAX_ZIP_COMMENT_BYTES: usize = u16::MAX as usize;
const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
const ZIP64_EOCD_SIGNATURE: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR_SIGNATURE: &[u8; 4] = b"PK\x06\x07";

/// Bounds used by one OOXML consumer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundedZipLimits {
    label: &'static str,
    max_input_bytes: usize,
    max_member_bytes: usize,
    max_total_read_bytes: usize,
}

impl BoundedZipLimits {
    pub(crate) const fn new(
        label: &'static str,
        max_input_bytes: usize,
        max_member_bytes: usize,
        max_total_read_bytes: usize,
    ) -> Self {
        Self {
            label,
            max_input_bytes,
            max_member_bytes,
            max_total_read_bytes,
        }
    }
}

/// A canonical, non-encrypted ZIP whose entries can be read under explicit
/// per-member and aggregate limits.
pub(crate) struct BoundedZip<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    names: Vec<String>,
    limits: BoundedZipLimits,
    total_read_bytes: usize,
}

impl<'a> BoundedZip<'a> {
    pub(crate) fn open(bytes: &'a [u8], limits: BoundedZipLimits) -> Result<Self> {
        validate_zip_envelope(bytes, limits)?;
        let config = Config {
            archive_offset: ArchiveOffset::Known(0),
        };
        let mut archive = ZipArchive::with_config(config, Cursor::new(bytes)).map_err(|err| {
            contract(
                limits.label,
                format!("ZIP central directory is invalid: {err}"),
            )
        })?;
        if archive.len() > MAX_ZIP_ENTRIES {
            return Err(contract(
                limits.label,
                format!(
                    "declares {} ZIP entries, over the {MAX_ZIP_ENTRIES} bound",
                    archive.len()
                ),
            ));
        }

        let mut names = Vec::with_capacity(archive.len());
        let mut seen = HashSet::with_capacity(archive.len());
        for index in 0..archive.len() {
            let file = archive.by_index(index).map_err(|err| {
                contract(
                    limits.label,
                    format!("ZIP member {index} is invalid: {err}"),
                )
            })?;
            let name = std::str::from_utf8(file.name_raw())
                .map_err(|_| contract(limits.label, "ZIP member name is not UTF-8".to_owned()))?;
            validate_part_name(name, limits.label)?;
            if !seen.insert(name.to_owned()) {
                return Err(contract(
                    limits.label,
                    format!("ZIP contains duplicate member `{name}`"),
                ));
            }
            if file.encrypted() {
                return Err(contract(
                    limits.label,
                    format!("ZIP member `{name}` is encrypted"),
                ));
            }
            if file.is_symlink() {
                return Err(contract(
                    limits.label,
                    format!("ZIP member `{name}` is a symbolic link"),
                ));
            }
            if !file.is_dir()
                && !matches!(
                    file.compression(),
                    CompressionMethod::Stored | CompressionMethod::Deflated
                )
            {
                return Err(contract(
                    limits.label,
                    format!("ZIP member `{name}` uses unsupported compression"),
                ));
            }
            if file.size() > limits.max_member_bytes as u64 {
                return Err(contract(
                    limits.label,
                    format!(
                        "ZIP member `{name}` declares {} bytes, over the {} byte bound",
                        file.size(),
                        limits.max_member_bytes
                    ),
                ));
            }
            names.push(name.to_owned());
        }

        Ok(Self {
            archive,
            names,
            limits,
            total_read_bytes: 0,
        })
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|member| member == name)
    }

    pub(crate) fn member_names(&self) -> &[String] {
        &self.names
    }

    pub(crate) fn read_required(&mut self, name: &str) -> Result<Vec<u8>> {
        if !self.contains(name) {
            return Err(contract(
                self.limits.label,
                format!("has no required ZIP member `{name}`"),
            ));
        }
        self.read_member(name)?.ok_or_else(|| {
            contract(
                self.limits.label,
                format!("has no required ZIP member `{name}`"),
            )
        })
    }

    pub(crate) fn read_optional(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        if !self.contains(name) {
            return Ok(None);
        }
        self.read_member(name)
    }

    fn read_member(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let file = self.archive.by_name(name).map_err(|err| {
            contract(
                self.limits.label,
                format!("ZIP member `{name}` cannot be opened: {err}"),
            )
        })?;
        if file.is_dir() {
            return Ok(None);
        }
        let declared = usize::try_from(file.size()).map_err(|_| {
            contract(
                self.limits.label,
                format!("ZIP member `{name}` has an unrepresentable size"),
            )
        })?;
        let next_total = self
            .total_read_bytes
            .checked_add(declared)
            .ok_or_else(|| contract(self.limits.label, "ZIP read total overflows".to_owned()))?;
        if next_total > self.limits.max_total_read_bytes {
            return Err(contract(
                self.limits.label,
                format!(
                    "ZIP reads would exceed the {} byte aggregate bound",
                    self.limits.max_total_read_bytes
                ),
            ));
        }
        let mut bytes = Vec::with_capacity(declared);
        let mut bounded = file.take(u64::try_from(declared).expect("usize always fits u64") + 1);
        bounded.read_to_end(&mut bytes).map_err(|err| {
            contract(
                self.limits.label,
                format!("ZIP member `{name}` failed CRC-checked read: {err}"),
            )
        })?;
        if bytes.len() != declared {
            return Err(contract(
                self.limits.label,
                format!("ZIP member `{name}` did not match its declared size"),
            ));
        }
        self.total_read_bytes = next_total;
        Ok(Some(bytes))
    }
}

/// Validate a DOCX or PPTX package before handing it to a real renderer.
pub(crate) fn validate_real_office_package(input: &[u8], media_type: &str) -> Result<()> {
    const MAX_OFFICE_MEMBER_BYTES: usize = 32 * 1024 * 1024;
    const MAX_OFFICE_REQUIRED_READ_BYTES: usize = 6 * 1024 * 1024;
    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

    let (main_part, content_type, root_name, root_namespace) = match media_type {
        DOCX => (
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            b"w:document".as_slice(),
            "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
        ),
        PPTX => (
            "ppt/presentation.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
            b"p:presentation".as_slice(),
            "http://schemas.openxmlformats.org/presentationml/2006/main",
        ),
        _ => {
            return Err(contract(
                "Office",
                format!("unsupported OOXML media type `{media_type}`"),
            ));
        }
    };
    let mut archive = BoundedZip::open(
        input,
        BoundedZipLimits::new(
            "Office",
            100 * 1024 * 1024,
            MAX_OFFICE_MEMBER_BYTES,
            MAX_OFFICE_REQUIRED_READ_BYTES,
        ),
    )?;
    let content_types = archive.read_required("[Content_Types].xml")?;
    let root_rels = archive.read_required("_rels/.rels")?;
    let main_xml = archive.read_required(main_part)?;
    xml::validate_content_type(&content_types, main_part, content_type)?;
    xml::validate_root_relationship(&root_rels, main_part)?;
    xml::validate_xml_root(&main_xml, root_name, root_namespace, main_part)
}

fn validate_zip_envelope(bytes: &[u8], limits: BoundedZipLimits) -> Result<()> {
    let label = limits.label;
    if bytes.len() > limits.max_input_bytes {
        return Err(contract(
            label,
            format!(
                "ZIP input is {} bytes, over the {} byte bound",
                bytes.len(),
                limits.max_input_bytes
            ),
        ));
    }
    if bytes.len() < EOCD_LEN || !bytes.starts_with(b"PK") {
        return Err(contract(label, "is not a ZIP container".to_owned()));
    }
    let scan_start = bytes.len().saturating_sub(EOCD_LEN + MAX_ZIP_COMMENT_BYTES);
    let mut eocd = None;
    for offset in (scan_start..=bytes.len() - EOCD_LEN).rev() {
        if bytes[offset..offset + 4] != *EOCD_SIGNATURE {
            continue;
        }
        let comment_len = read_u16(bytes, offset + 20, label)? as usize;
        if offset + EOCD_LEN + comment_len == bytes.len() && eocd.replace(offset).is_some() {
            return Err(contract(label, "has ambiguous ZIP end records".to_owned()));
        }
    }
    let eocd = eocd.ok_or_else(|| contract(label, "has no canonical ZIP end record".to_owned()))?;
    let disk = read_u16(bytes, eocd + 4, label)?;
    let central_disk = read_u16(bytes, eocd + 6, label)?;
    let disk_entries = read_u16(bytes, eocd + 8, label)?;
    let total_entries = read_u16(bytes, eocd + 10, label)?;
    let central_size = read_u32(bytes, eocd + 12, label)?;
    let central_offset = read_u32(bytes, eocd + 16, label)?;
    if disk != 0 || central_disk != 0 || disk_entries != total_entries {
        return Err(contract(label, "uses a multi-disk ZIP layout".to_owned()));
    }
    if total_entries == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(contract(
            label,
            "uses unsupported ZIP64 metadata".to_owned(),
        ));
    }
    if total_entries as usize > MAX_ZIP_ENTRIES {
        return Err(contract(
            label,
            format!("declares {total_entries} ZIP entries, over the {MAX_ZIP_ENTRIES} bound"),
        ));
    }
    if eocd >= 20 && bytes[eocd - 20..eocd - 16] == *ZIP64_LOCATOR_SIGNATURE {
        return Err(contract(
            label,
            "uses unsupported ZIP64 metadata".to_owned(),
        ));
    }
    if eocd >= 12 && bytes[eocd - 12..eocd - 8] == *ZIP64_EOCD_SIGNATURE {
        return Err(contract(
            label,
            "uses unsupported ZIP64 metadata".to_owned(),
        ));
    }
    let central_end = usize::try_from(central_offset)
        .ok()
        .and_then(|offset| offset.checked_add(central_size as usize))
        .ok_or_else(|| contract(label, "central directory offset overflows".to_owned()))?;
    if central_end != eocd {
        return Err(contract(
            label,
            "central directory is not contiguous with the end record".to_owned(),
        ));
    }
    validate_central_metadata(
        bytes,
        central_offset as usize,
        central_end,
        total_entries as usize,
        limits,
    )?;
    Ok(())
}

/// Check central and local-header agreement before the ZIP crate builds its
/// in-memory index. This is deliberately metadata-only: decompression and CRC
/// verification remain the maintained reader's responsibility.
fn validate_central_metadata(
    bytes: &[u8],
    central_offset: usize,
    central_end: usize,
    entry_count: usize,
    limits: BoundedZipLimits,
) -> Result<()> {
    const CENTRAL_HEADER_LEN: usize = 46;
    let mut cursor = central_offset;
    let mut names = HashSet::with_capacity(entry_count);
    let mut declared_total = 0usize;
    for _ in 0..entry_count {
        if bytes.get(cursor..cursor + 4) != Some(&b"PK\x01\x02"[..]) {
            return Err(contract(
                limits.label,
                "central directory entry is malformed",
            ));
        }
        let flags = read_u16(bytes, cursor + 8, limits.label)?;
        let compression = read_u16(bytes, cursor + 10, limits.label)?;
        let crc = read_u32(bytes, cursor + 16, limits.label)?;
        let compressed_size = read_u32(bytes, cursor + 20, limits.label)?;
        let uncompressed_size = read_u32(bytes, cursor + 24, limits.label)?;
        let name_len = read_u16(bytes, cursor + 28, limits.label)? as usize;
        let extra_len = read_u16(bytes, cursor + 30, limits.label)? as usize;
        let comment_len = read_u16(bytes, cursor + 32, limits.label)? as usize;
        let disk_start = read_u16(bytes, cursor + 34, limits.label)?;
        let local_offset = read_u32(bytes, cursor + 42, limits.label)? as usize;
        if compressed_size == u32::MAX || uncompressed_size == u32::MAX {
            return Err(contract(
                limits.label,
                "uses unsupported ZIP64 member metadata",
            ));
        }
        if flags & 1 != 0 || disk_start != 0 {
            return Err(contract(
                limits.label,
                "uses encrypted or split ZIP members",
            ));
        }
        if !matches!(compression, 0 | 8) {
            return Err(contract(
                limits.label,
                format!("ZIP member uses unsupported compression method {compression}"),
            ));
        }
        if uncompressed_size as usize > limits.max_member_bytes {
            return Err(contract(
                limits.label,
                format!(
                    "ZIP member declares {uncompressed_size} bytes, over the {} byte bound",
                    limits.max_member_bytes
                ),
            ));
        }
        declared_total = declared_total
            .checked_add(uncompressed_size as usize)
            .ok_or_else(|| contract(limits.label, "ZIP declared size total overflows"))?;
        if declared_total > MAX_ZIP_DECLARED_BYTES {
            return Err(contract(
                limits.label,
                "ZIP declared size total exceeds the 256 MiB bound",
            ));
        }
        let name_start = cursor + CENTRAL_HEADER_LEN;
        let name_end = name_start.checked_add(name_len).ok_or_else(|| {
            contract(
                limits.label,
                "central directory name offset overflows".to_owned(),
            )
        })?;
        let name = std::str::from_utf8(
            bytes
                .get(name_start..name_end)
                .ok_or_else(|| contract(limits.label, "central directory name is truncated"))?,
        )
        .map_err(|_| contract(limits.label, "ZIP member name is not UTF-8"))?;
        validate_part_name(name, limits.label)?;
        if !names.insert(name.to_owned()) {
            return Err(contract(
                limits.label,
                format!("ZIP contains duplicate member `{name}`"),
            ));
        }
        validate_local_header(
            bytes,
            local_offset,
            name.as_bytes(),
            flags,
            compression,
            crc,
            compressed_size,
            uncompressed_size,
            central_offset,
            limits.label,
        )?;
        cursor = name_end
            .checked_add(extra_len)
            .and_then(|offset| offset.checked_add(comment_len))
            .ok_or_else(|| contract(limits.label, "central directory offset overflows"))?;
    }
    if cursor != central_end {
        return Err(contract(
            limits.label,
            "central directory entry count does not match its size",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_local_header(
    bytes: &[u8],
    local_offset: usize,
    central_name: &[u8],
    central_flags: u16,
    central_compression: u16,
    central_crc: u32,
    central_compressed_size: u32,
    central_uncompressed_size: u32,
    central_offset: usize,
    label: &str,
) -> Result<()> {
    const LOCAL_HEADER_LEN: usize = 30;
    if local_offset >= bytes.len() {
        return Err(contract(label, "ZIP member local header is out of bounds"));
    }
    if bytes.get(local_offset..local_offset + 4) != Some(&b"PK\x03\x04"[..]) {
        return Err(contract(label, "ZIP member has no matching local header"));
    }
    let flags = read_u16(bytes, local_offset + 6, label)?;
    let compression = read_u16(bytes, local_offset + 8, label)?;
    let crc = read_u32(bytes, local_offset + 14, label)?;
    let compressed_size = read_u32(bytes, local_offset + 18, label)?;
    let uncompressed_size = read_u32(bytes, local_offset + 22, label)?;
    let name_len = read_u16(bytes, local_offset + 26, label)? as usize;
    let extra_len = read_u16(bytes, local_offset + 28, label)? as usize;
    let local_name_end = local_offset
        .checked_add(LOCAL_HEADER_LEN)
        .and_then(|offset| offset.checked_add(name_len))
        .ok_or_else(|| contract(label, "local ZIP header offset overflows"))?;
    if bytes.get(local_offset + LOCAL_HEADER_LEN..local_name_end) != Some(central_name) {
        return Err(contract(
            label,
            "ZIP central and local member names disagree",
        ));
    }
    let data_start = local_name_end
        .checked_add(extra_len)
        .ok_or_else(|| contract(label, "local ZIP header offset overflows"))?;
    if data_start > bytes.len() || flags != central_flags || compression != central_compression {
        return Err(contract(
            label,
            "ZIP central and local member headers disagree",
        ));
    }
    let data_end = data_start
        .checked_add(central_compressed_size as usize)
        .ok_or_else(|| contract(label, "ZIP member data offset overflows"))?;
    if data_end > central_offset {
        return Err(contract(
            label,
            "ZIP member data overlaps the central directory",
        ));
    }
    if flags & (1 << 3) == 0
        && (crc != central_crc
            || compressed_size != central_compressed_size
            || uncompressed_size != central_uncompressed_size)
    {
        return Err(contract(
            label,
            "ZIP central and local member sizes disagree",
        ));
    }
    Ok(())
}

fn validate_part_name(name: &str, label: &str) -> Result<()> {
    let directory = name.ends_with('/');
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.contains('\0')
        || (directory && name.ends_with("//"))
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(contract(
            label,
            format!("ZIP member has a non-canonical path `{name}`"),
        ));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize, label: &str) -> Result<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|slice| u16::from_le_bytes([slice[0], slice[1]]))
        .ok_or_else(|| contract(label, "ZIP structure is truncated".to_owned()))
}

fn read_u32(bytes: &[u8], offset: usize, label: &str) -> Result<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|slice| u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
        .ok_or_else(|| contract(label, "ZIP structure is truncated".to_owned()))
}

fn contract(label: &str, detail: impl Into<String>) -> AdapterError {
    AdapterError::ContractViolation(format!("{label} {}", detail.into()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use zip::{ZipWriter, write::SimpleFileOptions};

    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
    const CONTENT_TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    const ROOT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="r1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    const DOCUMENT: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#;

    fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in members {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .expect("start member");
            writer.write_all(contents).expect("write member");
        }
        writer.finish().expect("finish fixture").into_inner()
    }

    fn docx() -> Vec<u8> {
        zip_of(&[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            ("word/document.xml", DOCUMENT.as_bytes()),
        ])
    }

    #[test]
    fn office_preflight_accepts_a_valid_docx_package() {
        validate_real_office_package(&docx(), DOCX).expect("valid DOCX");
    }

    #[test]
    fn office_preflight_refuses_plaintext_and_wrong_kind() {
        assert!(validate_real_office_package(b"renamed plaintext", DOCX).is_err());
        assert!(validate_real_office_package(&docx(), PPTX).is_err());
    }

    #[test]
    fn office_preflight_requires_the_declared_main_part_and_root() {
        let missing = zip_of(&[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
        ]);
        assert!(validate_real_office_package(&missing, DOCX).is_err());
        let wrong_root = zip_of(&[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            ("word/document.xml", b"<html/>"),
        ]);
        assert!(validate_real_office_package(&wrong_root, DOCX).is_err());
    }

    #[test]
    fn bounded_zip_refuses_corrupt_crc_and_truncated_input() {
        let mut corrupt = docx();
        let body = corrupt
            .windows(DOCUMENT.len())
            .position(|window| window == DOCUMENT.as_bytes())
            .expect("fixture body");
        corrupt[body] ^= 1;
        assert!(validate_real_office_package(&corrupt, DOCX).is_err());
        let mut truncated = docx();
        truncated.pop();
        assert!(validate_real_office_package(&truncated, DOCX).is_err());
    }

    #[test]
    fn bounded_zip_refuses_duplicate_paths_and_oversized_metadata() {
        let mut duplicate = zip_of(&[
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", ROOT_RELS.as_bytes()),
            ("word/document.xml", DOCUMENT.as_bytes()),
            ("word/document.xmZ", DOCUMENT.as_bytes()),
        ]);
        let position = duplicate
            .windows(b"word/document.xmZ".len())
            .rposition(|window| window == b"word/document.xmZ")
            .expect("central-directory member name");
        duplicate[position..position + b"word/document.xmZ".len()]
            .copy_from_slice(b"word/document.xml");
        assert!(validate_real_office_package(&duplicate, DOCX).is_err());

        let mut oversized = docx();
        let central = oversized
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("central directory");
        oversized[central + 24..central + 28]
            .copy_from_slice(&((32 * 1024 * 1024 + 1) as u32).to_le_bytes());
        assert!(validate_real_office_package(&oversized, DOCX).is_err());
    }
}
