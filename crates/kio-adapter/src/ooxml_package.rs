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

pub(crate) mod xml;

use kio_core::error::{Resource, ResourceLimit, ResourceTier};

const MAX_ZIP_ENTRIES: usize = 4_096;
const MAX_ZIP_DECLARED_BYTES: u64 = 256 * 1024 * 1024;
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
    max_total_read_bytes: u64,
    max_declared_bytes: u64,
    max_entries: usize,
    tier: Option<ResourceTier>,
}

impl BoundedZipLimits {
    pub(crate) const fn new(
        label: &'static str,
        max_input_bytes: usize,
        max_member_bytes: usize,
        max_total_read_bytes: u64,
    ) -> Self {
        Self {
            label,
            max_input_bytes,
            max_member_bytes,
            max_total_read_bytes,
            max_declared_bytes: MAX_ZIP_DECLARED_BYTES,
            max_entries: MAX_ZIP_ENTRIES,
            tier: None,
        }
    }

    pub(crate) const fn xlsx(tier: ResourceTier) -> Self {
        const MIB: usize = 1024 * 1024;
        let (raw, member, aggregate) = match tier {
            ResourceTier::Standard => (100 * MIB, 64 * MIB, 256 * MIB as u64),
            ResourceTier::Large => (1024 * MIB, 256 * MIB, 4096 * MIB as u64),
        };
        Self {
            label: "XLSX",
            max_input_bytes: raw,
            max_member_bytes: member,
            max_total_read_bytes: aggregate,
            max_declared_bytes: aggregate,
            max_entries: u16::MAX as usize - 1,
            tier: Some(tier),
        }
    }

    pub(crate) const fn raw_bytes(self) -> u64 {
        self.max_input_bytes as u64
    }

    pub(crate) fn check_raw_bytes(self, bytes: u64) -> Result<()> {
        self.check(Resource::RawBytes, self.raw_bytes(), bytes)
    }

    fn check(self, resource: Resource, limit: u64, observed: u64) -> Result<()> {
        if observed > limit {
            return Err(match self.tier {
                Some(tier) => AdapterError::ResourceLimit(ResourceLimit {
                    resource,
                    stage: "xlsx_zip".to_owned(),
                    limit: Some(limit),
                    observed: Some(observed),
                    resource_tier: Some(tier),
                    page_progress: None,
                }),
                None => contract(
                    self.label,
                    format!("ZIP {resource:?} {observed} exceeds {limit}"),
                ),
            });
        }
        Ok(())
    }
}

/// A canonical, non-encrypted ZIP whose entries can be read under explicit
/// per-member and aggregate limits.
pub(crate) struct BoundedZip<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    names: Vec<String>,
    limits: BoundedZipLimits,
    total_read_bytes: u64,
}

impl<'a> BoundedZip<'a> {
    pub(crate) fn open(bytes: &'a [u8], limits: BoundedZipLimits) -> Result<Self> {
        Self::open_accounted(bytes, limits, |_, _| Ok(()))
    }

    pub(crate) fn open_accounted(
        bytes: &'a [u8],
        limits: BoundedZipLimits,
        admit_index: impl FnOnce(usize, usize) -> Result<()>,
    ) -> Result<Self> {
        validate_zip_envelope(bytes, limits, admit_index)?;
        let config = Config {
            archive_offset: ArchiveOffset::Known(0),
        };
        let mut archive = ZipArchive::with_config(config, Cursor::new(bytes)).map_err(|err| {
            contract(
                limits.label,
                format!("ZIP central directory is invalid: {err}"),
            )
        })?;
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
        self.archive.index_for_name(name).is_some()
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
        self.consume(name, true, |_| Ok(()))?.ok_or_else(|| {
            contract(
                self.limits.label,
                format!("has no required ZIP member `{name}`"),
            )
        })
    }

    /// Check the decompressed size before admitting parser storage and work.
    /// When the caller only needs integrity, read into a fixed buffer.
    pub(crate) fn consume(
        &mut self,
        name: &str,
        retain: bool,
        admit: impl FnOnce(usize) -> Result<()>,
    ) -> Result<Option<Vec<u8>>> {
        let self_label = self.limits.label;
        self.read_member(name, |input, length| {
            admit(length)?;
            if retain {
                let mut bytes = vec![0; length];
                input
                    .read_exact(&mut bytes)
                    .map_err(|err| read_error(self_label, name, err))?;
                Ok(bytes)
            } else {
                std::io::copy(input, &mut std::io::sink())
                    .map_err(|err| read_error(self_label, name, err))?;
                Ok(Vec::new())
            }
        })
        .map(|bytes| bytes.filter(|_| retain))
    }

    pub(crate) fn read_member<T>(
        &mut self,
        name: &str,
        parse: impl FnOnce(&mut dyn Read, usize) -> Result<T>,
    ) -> Result<Option<T>> {
        let mut file = self.archive.by_name(name).map_err(|err| {
            contract(
                self.limits.label,
                format!("ZIP member `{name}` cannot be opened: {err}"),
            )
        })?;
        if file.is_dir() && self.limits.tier.is_none() {
            return Ok(None);
        }
        let declared = file.size();
        self.limits.check(
            Resource::MemberBytes,
            self.limits.max_member_bytes as u64,
            declared,
        )?;
        let next_total = self
            .total_read_bytes
            .checked_add(declared)
            .ok_or_else(|| contract(self.limits.label, "ZIP read total overflows"))?;
        self.limits.check(
            Resource::AggregateBytes,
            self.limits.max_total_read_bytes,
            next_total,
        )?;
        let length = usize::try_from(declared)
            .map_err(|_| contract(self.limits.label, "ZIP member size is unrepresentable"))?;

        let mut input = (&mut file).take(declared);
        let result = parse(&mut input, length)?;
        if input.limit() != 0
            || file
                .read(&mut [0])
                .map_err(|err| read_error(self.limits.label, name, err))?
                != 0
        {
            return Err(contract(
                self.limits.label,
                format!("ZIP member `{name}` did not match its declared size"),
            ));
        }
        self.total_read_bytes = next_total;
        Ok(Some(result))
    }
}

fn read_error(label: &str, name: &str, error: std::io::Error) -> AdapterError {
    contract(
        label,
        format!("ZIP member `{name}` failed CRC-checked read: {error}"),
    )
}
/// Validate a DOCX or PPTX package before handing it to a real renderer.
pub(crate) fn validate_real_office_package(input: &[u8], media_type: &str) -> Result<()> {
    const MAX_OFFICE_MEMBER_BYTES: usize = 32 * 1024 * 1024;
    const MAX_OFFICE_REQUIRED_READ_BYTES: u64 = 6 * 1024 * 1024;
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

fn validate_zip_envelope(
    bytes: &[u8],
    limits: BoundedZipLimits,
    admit_index: impl FnOnce(usize, usize) -> Result<()>,
) -> Result<()> {
    let label = limits.label;
    limits.check_raw_bytes(bytes.len() as u64)?;
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
    if total_entries as usize > limits.max_entries {
        return Err(contract(
            label,
            format!(
                "declares {total_entries} ZIP entries, over the {} bound",
                limits.max_entries
            ),
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
    admit_index(central_size as usize, total_entries as usize)?;
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
    let mut declared_total = 0u64;
    let mut largest_member = 0u64;
    let mut member_ranges = Vec::new();
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
        if flags & (1 | (1 << 6) | (1 << 13)) != 0 || disk_start != 0 {
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
        largest_member = largest_member.max(u64::from(uncompressed_size));
        declared_total = declared_total
            .checked_add(u64::from(uncompressed_size))
            .ok_or_else(|| contract(limits.label, "ZIP declared size total overflows"))?;
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
        if !names.insert(name) {
            return Err(contract(
                limits.label,
                format!("ZIP contains duplicate member `{name}`"),
            ));
        }
        let member_end = validate_local_header(
            bytes,
            local_offset,
            name.as_bytes(),
            flags,
            compression,
            crc,
            compressed_size,
            uncompressed_size,
            central_offset,
            limits,
        )?;
        let extra_end = name_end
            .checked_add(extra_len)
            .filter(|end| *end <= central_end)
            .ok_or_else(|| {
                contract(limits.label, "central directory extra fields are truncated")
            })?;
        if limits.tier.is_some() {
            validate_extra(&bytes[name_end..extra_end], name.as_bytes(), limits.label)?;
            if name.contains(':') || name.chars().any(char::is_control) {
                return Err(contract(
                    limits.label,
                    "ZIP part name contains an unsafe character",
                ));
            }
            member_ranges.push((local_offset, member_end));
        }
        cursor = extra_end
            .checked_add(comment_len)
            .ok_or_else(|| contract(limits.label, "central directory offset overflows"))?;
    }
    if cursor != central_end {
        return Err(contract(
            limits.label,
            "central directory entry count does not match its size",
        ));
    }
    member_ranges.sort_unstable();
    if member_ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(contract(limits.label, "ZIP members overlap"));
    }
    limits.check(
        Resource::MemberBytes,
        limits.max_member_bytes as u64,
        largest_member,
    )?;
    limits.check(
        Resource::AggregateBytes,
        limits.max_declared_bytes,
        declared_total,
    )
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
    limits: BoundedZipLimits,
) -> Result<usize> {
    let label = limits.label;
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
    let uses_descriptor = flags & (1 << 3) != 0;
    let local = [crc, compressed_size, uncompressed_size];
    let central = [
        central_crc,
        central_compressed_size,
        central_uncompressed_size,
    ];
    if !uses_descriptor && local != central {
        return Err(contract(
            label,
            "ZIP central and local member sizes or CRC disagree",
        ));
    }
    if limits.tier.is_none() {
        return Ok(data_end);
    }
    validate_extra(&bytes[local_name_end..data_start], central_name, label)?;
    if !uses_descriptor {
        return Ok(data_end);
    }
    if local
        .iter()
        .zip(central)
        .any(|(&value, expected)| value != 0 && value != expected)
    {
        return Err(contract(
            label,
            "ZIP local header disagrees with its data descriptor",
        ));
    }
    // The ZIP descriptor signature is optional. A CRC can itself equal the
    // signature, so recognize either exact, bounded descriptor layout.
    for offset in [
        Some(data_end),
        (bytes.get(data_end..data_end + 4) == Some(&b"PK\x07\x08"[..])).then_some(data_end + 4),
    ]
    .into_iter()
    .flatten()
    {
        if offset + 12 <= central_offset
            && read_u32(bytes, offset, label)? == central_crc
            && read_u32(bytes, offset + 4, label)? == central_compressed_size
            && read_u32(bytes, offset + 8, label)? == central_uncompressed_size
        {
            return Ok(offset + 12);
        }
    }
    Err(contract(
        label,
        "ZIP data descriptor disagrees with the central directory",
    ))
}

fn validate_extra(mut extra: &[u8], name: &[u8], label: &str) -> Result<()> {
    while !extra.is_empty() {
        let id = read_u16(extra, 0, label)?;
        let len = usize::from(read_u16(extra, 2, label)?);
        if id == 1 {
            return Err(contract(label, "uses unsupported ZIP64 extra fields"));
        }
        if id == 0x7075 && extra.get(9..4 + len) != Some(name) {
            return Err(contract(
                label,
                "ZIP Unicode path changes the UTF-8 member name",
            ));
        }
        extra = extra
            .get(4 + len..)
            .ok_or_else(|| contract(label, "ZIP extra field is truncated"))?;
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

    #[test]
    fn xlsx_zip_budgets_admit_exact_boundaries() {
        let input = zip_of(&[("a", b"1234"), ("b", b"567")]);
        let exact = BoundedZipLimits {
            max_input_bytes: input.len(),
            max_member_bytes: 4,
            max_declared_bytes: 7,
            max_total_read_bytes: 7,
            ..BoundedZipLimits::xlsx(ResourceTier::Standard)
        };
        let mut archive = BoundedZip::open(&input, exact).unwrap();
        assert_eq!(archive.read_required("a").unwrap(), b"1234");
        assert_eq!(archive.read_required("b").unwrap(), b"567");
        assert!(
            matches!(archive.read_required("b"), Err(AdapterError::ResourceLimit(limit))
            if limit.resource == Resource::AggregateBytes && limit.observed == Some(10))
        );

        for (limits, resource, observed) in [
            (
                BoundedZipLimits {
                    max_input_bytes: input.len() - 1,
                    ..exact
                },
                Resource::RawBytes,
                input.len() as u64,
            ),
            (
                BoundedZipLimits {
                    max_member_bytes: 3,
                    ..exact
                },
                Resource::MemberBytes,
                4,
            ),
            (
                BoundedZipLimits {
                    max_declared_bytes: 6,
                    ..exact
                },
                Resource::AggregateBytes,
                7,
            ),
        ] {
            assert!(
                matches!(BoundedZip::open(&input, limits), Err(AdapterError::ResourceLimit(limit))
                if limit.resource == resource && limit.observed == Some(observed))
            );
        }
    }

    #[test]
    fn xlsx_zip_checks_streaming_descriptors_and_payload_crc() {
        let mut writer = ZipWriter::new_stream(Vec::new());
        writer
            .start_file(
                "member",
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"member-body").unwrap();
        let input = writer.finish().unwrap().into_inner();
        let limits = BoundedZipLimits::xlsx(ResourceTier::Standard);
        let mut archive = BoundedZip::open(&input, limits).unwrap();
        assert_eq!(archive.read_required("member").unwrap(), b"member-body");

        let mut wrong_crc = input.clone();
        let offset = input
            .windows(b"member-body".len())
            .position(|b| b == b"member-body")
            .unwrap();
        wrong_crc[offset] ^= 1;
        let mut archive = BoundedZip::open(&wrong_crc, limits).unwrap();
        assert!(matches!(
            archive.consume("member", false, |_| Ok(())),
            Err(AdapterError::ContractViolation(_))
        ));

        let mut wrong_descriptor = input;
        let offset = wrong_descriptor
            .windows(4)
            .position(|b| b == b"PK\x07\x08")
            .unwrap();
        wrong_descriptor[offset + 4] ^= 1;
        assert!(matches!(
            BoundedZip::open(&wrong_descriptor, limits),
            Err(AdapterError::ContractViolation(_))
        ));
    }

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
