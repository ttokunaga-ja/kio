//! Chunking contracts for normalized unit instances.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::{ChunkRow, IndexError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkingConfig {
    pub chunking_config_hash: String,
    /// 03 §11 `[chunking] strategy` (MVP は "heading" のみ)
    pub strategy: String,
    /// 03 §11 `[chunking] max_chars` (04 §4.1 の分割規則の閾値)
    pub max_chars: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedUnitInput {
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub unit_key: String,
    /// Hash of exact normalized Markdown bytes. A missing value is rejected.
    pub unit_content_hash: String,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkingInput {
    pub raw_path: String,
    pub units: Vec<NormalizedUnitInput>,
    pub config: ChunkingConfig,
    pub created_at: String,
}

pub fn default_chunking_config() -> Result<ChunkingConfig> {
    let hash = chunking_config_hash("heading", 6000)?;
    Ok(ChunkingConfig {
        chunking_config_hash: hash,
        strategy: "heading".to_owned(),
        max_chars: 6000,
    })
}

pub fn chunking_config_hash(strategy: &str, max_chars: u64) -> Result<String> {
    kio_core::dag::chunking_config_hash(strategy, max_chars)
        .map_err(|error| IndexError::Contract(error.to_string()))
}

pub fn chunk_hash(row: &ChunkRow) -> Result<String> {
    validate_unit_hash("unit_content_hash", &row.unit_content_hash)?;
    let mut map = Map::new();
    map.insert("byte_end".to_owned(), json!(row.byte_end));
    map.insert("byte_start".to_owned(), json!(row.byte_start));
    map.insert("gen".to_owned(), json!(row.r#gen));
    map.insert(
        "heading_path".to_owned(),
        json!(row.heading_path.clone().unwrap_or_default()),
    );
    map.insert("raw_hash".to_owned(), json!(row.raw_hash));
    if let Some(section_id) = row.section_id.as_ref().filter(|value| !value.is_empty()) {
        map.insert("section_id".to_owned(), json!(section_id));
    }
    map.insert("spec_version".to_owned(), json!(1));
    map.insert("tool_profile_hash".to_owned(), json!(row.tool_profile_hash));
    map.insert("unit_key".to_owned(), json!(row.unit_key));
    map.insert("unit_content_hash".to_owned(), json!(row.unit_content_hash));
    hash_jcs(&Value::Object(map))
}

/// Require the canonical SHA-256 content-address spelling used for immutable
/// normalized-unit objects. Keeping this at the index boundary prevents a
/// malformed durable record from becoming a distinct chunk identity.
pub(crate) fn validate_unit_hash(label: &str, value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(IndexError::Contract(format!(
            "{label} must be sha256:<64 lowercase hex digits>"
        )));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(IndexError::Contract(format!(
            "{label} must be sha256:<64 lowercase hex digits>"
        )));
    }
    Ok(())
}

pub fn slugify_heading(text: &str) -> String {
    let normalized: String = text.nfc().collect();
    let mut out = String::with_capacity(normalized.len());
    let mut last_dash = false;
    for ch in normalized.chars() {
        let mapped = if ch.is_ascii_uppercase() {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_ascii_alphanumeric() || ch == '_' || is_japanese(ch) {
            Some(ch)
        } else if ch.is_whitespace() || ch == '-' {
            Some('-')
        } else {
            None
        };
        if let Some(ch) = mapped {
            if ch == '-' {
                if !last_dash {
                    out.push(ch);
                    last_dash = true;
                }
            } else {
                out.push(ch);
                last_dash = false;
            }
        }
    }
    out.trim_matches('-').to_owned()
}

pub fn chunk_normalized_instance(input: ChunkingInput) -> Result<Vec<ChunkRow>> {
    if input.config.strategy != "heading" {
        return Err(IndexError::Contract(format!(
            "unsupported chunking strategy: {}",
            input.config.strategy
        )));
    }
    if input.config.max_chars == 0 {
        return Err(IndexError::Contract(
            "chunking max_chars must be greater than zero".to_owned(),
        ));
    }

    let mut rows = Vec::new();
    for unit in &input.units {
        validate_unit_hash("unit_content_hash", &unit.unit_content_hash)?;
        // N6: materialize the unit's chars once so every span slice is an O(1)
        // index range instead of an O(offset) `chars().skip(start)` rescan. The
        // section/heading scan stays &str-based (already a single linear pass);
        // only the quadratic span slicing moves to the Vec<char>. Output bytes are
        // unchanged — `unit_chars[a..b]` collects the same String as before.
        let unit_chars: Vec<char> = unit.markdown.chars().collect();
        // 03 §8.1 / §8: the *stored* span is the unit-local UTF-8 byte offset
        // (0-based half-open), not the Unicode scalar (char) index used above for
        // max_chars accounting (04 §4.1 rule 5 fixes the split-boundary unit as
        // scalar value, independent of the persisted byte span). `char_indices()`
        // yields exactly `unit_chars.len()` byte offsets in the same order as
        // `chars()`, so this table converts a char index directly to its byte
        // offset in O(1) per lookup; the sentinel entry (total byte length) covers
        // an `end` that lands on the unit's tail.
        let char_byte_offsets: Vec<usize> = unit
            .markdown
            .char_indices()
            .map(|(byte_offset, _)| byte_offset)
            .chain(std::iter::once(unit.markdown.len()))
            .collect();
        let sections = section_ranges(&unit.markdown);
        let mut duplicate_counts = BTreeMap::<String, u64>::new();
        for section in sections {
            let base_section_id = if section.heading_path.is_empty() {
                None
            } else {
                let joined = section
                    .heading_path
                    .iter()
                    .map(|heading| slugify_heading(heading))
                    .filter(|slug| !slug.is_empty())
                    .collect::<Vec<_>>()
                    .join("/");
                if joined.is_empty() {
                    None
                } else {
                    Some(joined)
                }
            };
            let section_id = base_section_id.map(|base| {
                let count = duplicate_counts.entry(base.clone()).or_insert(0);
                *count += 1;
                if *count == 1 {
                    base
                } else {
                    format!("{base}#{}", *count)
                }
            });
            for (start, end) in split_range_by_max_chars(
                &unit_chars,
                section.start,
                section.end,
                input.config.max_chars as usize,
            ) {
                if start >= end {
                    continue;
                }
                // `char_byte_offsets[i]` is always a valid UTF-8 char boundary (it
                // comes straight from `char_indices()` plus the whole-string
                // length sentinel), so slicing `unit.markdown` by the translated
                // byte range can never land mid-codepoint — this is an exact byte
                // copy of the span, not a re-encoding of collected chars.
                let byte_start = char_byte_offsets[start];
                let byte_end = char_byte_offsets[end];
                let text = unit.markdown[byte_start..byte_end].to_owned();
                // A span can be non-empty and still carry nothing: a document
                // that opens with blank lines yields a leading pre-heading
                // section of `\n\n\n`, which `start >= end` above does not
                // catch. Measured over the 1,015-document fixture corpus, 20
                // of 3,004 chunks were exactly that, every one `byte_start` 0
                // to `byte_end` 3.
                //
                // They are not harmless. Such a chunk is indexed, occupies one
                // of 05 §1.3's `candidate_depth` slots, can never legitimately
                // match a query, and collects a meaningless score from anything
                // that ranks it — a reranker scored 36 of them across 24
                // queries (`tasks/rerank-differential-plan.md (git 56a6831)` §2.9.3).
                //
                // Dropping one shifts nothing: spans come from section ranges
                // computed independently, and `section_id` numbering is
                // assigned per section before this loop.
                if text.trim().is_empty() {
                    continue;
                }
                let mut row = ChunkRow {
                    chunk_id: String::new(),
                    raw_hash: unit.raw_hash.clone(),
                    tool_profile_hash: unit.tool_profile_hash.clone(),
                    r#gen: unit.r#gen,
                    unit_key: unit.unit_key.clone(),
                    unit_content_hash: unit.unit_content_hash.clone(),
                    chunking_config_hash: input.config.chunking_config_hash.clone(),
                    raw_path: input.raw_path.clone(),
                    heading_path: Some(section.heading_path.clone()),
                    section_id: section_id.clone(),
                    byte_start: byte_start as u64,
                    byte_end: byte_end as u64,
                    text_hash: hash_bytes(text.as_bytes()),
                    text,
                    created_at: input.created_at.clone(),
                };
                row.chunk_id = chunk_hash(&row)?;
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

#[derive(Debug, Clone)]
struct SectionRange {
    start: usize,
    end: usize,
    heading_path: Vec<String>,
}

#[derive(Debug, Clone)]
struct HeadingEvent {
    start: usize,
    level: usize,
    text: String,
}

fn section_ranges(markdown: &str) -> Vec<SectionRange> {
    let headings = heading_events(markdown);
    let total = markdown.chars().count();
    if headings.is_empty() {
        return vec![SectionRange {
            start: 0,
            end: total,
            heading_path: Vec::new(),
        }];
    }

    let mut ranges = Vec::new();
    if headings[0].start > 0 {
        ranges.push(SectionRange {
            start: 0,
            end: headings[0].start,
            heading_path: Vec::new(),
        });
    }

    let mut stack: Vec<String> = Vec::new();
    for (index, event) in headings.iter().enumerate() {
        stack.truncate(event.level.saturating_sub(1));
        stack.push(event.text.clone());
        let end = headings
            .get(index + 1)
            .map(|next| next.start)
            .unwrap_or(total);
        ranges.push(SectionRange {
            start: event.start,
            end,
            heading_path: stack.clone(),
        });
    }
    ranges
}

/// A line that toggles fenced-code state.
///
/// Deliberately the simple form — the first non-space content is three
/// backticks or three tildes — rather than full CommonMark fence matching
/// (opening length, matching closer, info string). It is shared with
/// [`crate::search_projection`] so that the chunker and the search projection
/// cannot disagree about where code starts: a `#` the chunker declines to read
/// as a heading and a `\/` the projection declines to unescape have to be
/// inside the same fence, or one of the two is wrong about the document.
pub(crate) fn is_fence_delimiter(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn heading_events(markdown: &str) -> Vec<HeadingEvent> {
    let mut events = Vec::new();
    let mut char_offset = 0usize;
    let mut in_fence = false;
    for line in markdown.split_inclusive('\n') {
        if is_fence_delimiter(line) {
            in_fence = !in_fence;
        }
        if !in_fence && let Some((level, text)) = parse_atx_heading(line) {
            events.push(HeadingEvent {
                start: char_offset,
                level,
                text,
            });
        }
        char_offset += line.chars().count();
    }
    events
}

fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    let mut chars = trimmed.chars();
    let mut level = 0usize;
    while matches!(chars.clone().next(), Some('#')) {
        level += 1;
        chars.next();
    }
    if !(1..=6).contains(&level) {
        return None;
    }
    if !matches!(chars.next(), Some(ch) if ch.is_whitespace()) {
        return None;
    }
    let text = chars.as_str().trim();
    let text = text.trim_end_matches('#').trim().to_owned();
    (!text.is_empty()).then_some((level, text))
}

fn split_range_by_max_chars(
    chars: &[char],
    start: usize,
    end: usize,
    max_chars: usize,
) -> Vec<(usize, usize)> {
    if end.saturating_sub(start) <= max_chars {
        return vec![(start, end)];
    }
    let mut ranges = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let limit = (cursor + max_chars).min(end);
        if limit == end {
            ranges.push((cursor, end));
            break;
        }
        let window = slice_chars(chars, cursor, limit);
        let split_at = last_paragraph_boundary(&window)
            .filter(|boundary| *boundary > 0)
            .map(|boundary| cursor + boundary)
            .unwrap_or(limit);
        ranges.push((cursor, split_at));
        cursor = split_at;
        // Consume run-leading whitespace by direct index (was an O(cursor) slice
        // per single char advance — the dominant term of the old O(N²) cost).
        while cursor < end && chars[cursor].is_whitespace() {
            cursor += 1;
        }
    }
    ranges
}

fn last_paragraph_boundary(window: &str) -> Option<usize> {
    let mut prev_newline = false;
    let mut last = None;
    let mut offset = 0usize;
    for ch in window.chars() {
        offset += 1;
        if ch == '\n' {
            if prev_newline {
                last = Some(offset);
            }
            prev_newline = true;
        } else if !ch.is_whitespace() {
            prev_newline = false;
        }
    }
    last
}

fn slice_chars(chars: &[char], start: usize, end: usize) -> String {
    chars[start..end].iter().collect()
}

fn hash_jcs(value: &Value) -> Result<String> {
    serde_jcs::to_vec(value)
        .map(|bytes| hash_bytes(&bytes))
        .map_err(|err| IndexError::Schema(err.to_string()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", lower_hex(&digest))
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn is_japanese(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x309f | 0x30a0..=0x30ff | 0x4e00..=0x9fff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "sha256:74bcb92d8088c950e45e4c43563332da2ca1e04b25d6d4016aa43f830d4cca8a";
    const TOOL: &str = "sha256:e067e42e6634b8043f46a4b7f55257ab10ca6266be80cc47b6a68a5aacd2c8f0";
    const UNIT: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn fixture_row(r#gen: u64, section_id: Option<&str>, heading_path: Vec<&str>) -> ChunkRow {
        ChunkRow {
            chunk_id: String::new(),
            raw_hash: RAW.to_owned(),
            tool_profile_hash: TOOL.to_owned(),
            r#gen,
            unit_key: "page:12".to_owned(),
            unit_content_hash: UNIT.to_owned(),
            chunking_config_hash: "sha256:cfg".to_owned(),
            raw_path: "report.pdf".to_owned(),
            heading_path: Some(heading_path.into_iter().map(str::to_owned).collect()),
            section_id: section_id.map(str::to_owned),
            byte_start: 1200,
            byte_end: 1500,
            text_hash: "sha256:text".to_owned(),
            text: String::new(),
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn ct3_chunk_001_hash_vector_gen0_section_id() {
        let row = fixture_row(0, Some("認証仕様/api-token"), vec!["認証仕様", "API Token"]);
        assert_eq!(
            chunk_hash(&row).unwrap(),
            "sha256:a485028c5eb08b1d3f2466f298d5747968f053544cb62e231205737b8c42d46b"
        );
    }

    #[test]
    fn ct3_chunk_002_hash_vector_gen_changes_identity() {
        let row = fixture_row(3, Some("認証仕様/api-token"), vec!["認証仕様", "API Token"]);
        assert_eq!(
            chunk_hash(&row).unwrap(),
            "sha256:a35deaf388632d20ee99f65d9944455ca87552badcc28ca74afacb0680d7c746"
        );
    }

    #[test]
    fn ct3_chunk_003_null_section_id_is_omitted_heading_path_empty_stays() {
        let mut row = ChunkRow {
            chunk_id: String::new(),
            raw_hash: RAW.to_owned(),
            tool_profile_hash: TOOL.to_owned(),
            r#gen: 0,
            unit_key: "doc:1".to_owned(),
            unit_content_hash: UNIT.to_owned(),
            chunking_config_hash: "sha256:cfg".to_owned(),
            raw_path: "report.pdf".to_owned(),
            heading_path: Some(Vec::new()),
            section_id: None,
            byte_start: 0,
            byte_end: 600,
            text_hash: "sha256:text".to_owned(),
            text: String::new(),
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let omitted = chunk_hash(&row).unwrap();
        row.section_id = Some(String::new());
        assert_eq!(chunk_hash(&row).unwrap(), omitted);
        assert_eq!(
            omitted,
            "sha256:c2edc03222dd9bfbd2d70334aa71f0f568592f08edfaec5d2f7148417ff4189c"
        );
    }

    #[test]
    fn immutable_unit_content_hash_separates_otherwise_identical_chunk_ids() {
        let first = fixture_row(0, Some("認証仕様/api-token"), vec!["認証仕様", "API Token"]);
        let mut corrected = first.clone();
        corrected.unit_content_hash =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222".to_owned();

        assert_ne!(chunk_hash(&first).unwrap(), chunk_hash(&corrected).unwrap());
    }

    #[test]
    fn unit_content_hash_must_use_canonical_sha256_spelling() {
        let mut row = fixture_row(0, None, Vec::new());
        row.unit_content_hash = "sha256:ABC".to_owned();
        assert!(matches!(chunk_hash(&row), Err(IndexError::Contract(_))));
    }

    #[test]
    fn ct3_chunk_004_heading_chunking_slug_code_fence_and_max_chars() {
        let config = ChunkingConfig {
            chunking_config_hash: chunking_config_hash("heading", 40).unwrap(),
            strategy: "heading".to_owned(),
            max_chars: 40,
        };
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: "intro\n```text\n# not heading\n```\n# 認証仕様\npara one\n\npara two\n## API Token\nbody".to_owned(),
            }],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        assert!(rows.iter().any(|row| row.heading_path == Some(Vec::new())));
        assert!(
            rows.iter()
                .any(|row| row.section_id.as_deref() == Some("認証仕様"))
        );
        assert!(
            rows.iter()
                .any(|row| row.section_id.as_deref() == Some("認証仕様/api-token"))
        );
        assert!(
            !rows
                .iter()
                .any(|row| row.section_id.as_deref() == Some("not-heading"))
        );
    }

    /// A document that opens with blank lines must not index the blank run.
    ///
    /// Measured on the fixture corpus before this guard: 20 of 3,004 chunks
    /// were exactly `\n\n\n` at span 0-3, and a reranker was handed 36 of
    /// them across 24 queries. The chunk after them must keep its own span, so
    /// this also pins that dropping one shifts nothing.
    #[test]
    fn a_leading_blank_run_is_not_a_chunk() {
        let config = default_chunking_config().unwrap();
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: "\n\n\n# H\nbody".to_owned(),
            }],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        assert!(
            rows.iter().all(|row| !row.text.trim().is_empty()),
            "a content-free chunk was indexed: {:?}",
            rows.iter()
                .map(|r| (r.byte_start, r.byte_end, &r.text))
                .collect::<Vec<_>>()
        );
        assert_eq!(rows.len(), 1, "only the heading section survives");
        assert_eq!(rows[0].byte_start, 3, "the surviving span is unshifted");
    }

    #[test]
    fn ct3_chunk_005_spans_are_unit_local() {
        let config = default_chunking_config().unwrap();
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: "abc\n# H\nbody".to_owned(),
            }],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        assert_eq!(rows[0].byte_start, 0);
        assert_eq!(rows[0].byte_end, 4);
        assert_eq!(rows[1].byte_start, 4);
    }

    // CT3-CHUNK-005 (gap fill): the spec's Given is explicitly a unit "combined at
    // the tail side of a full-text view" — a single-unit fixture can't distinguish
    // "span is unit-local" from "span happens to start at 0 because there's only
    // one unit". A second, later unit must restart byte_start at 0 rather than
    // continuing from the first unit's end, and its heading stack must not inherit
    // the first unit's headings (chunk does not cross the unit boundary, 04 §4.1
    // rule 1 / A.1 offset independence).
    #[test]
    fn ct3_chunk_005_second_unit_span_and_heading_path_do_not_inherit_from_first() {
        let config = default_chunking_config().unwrap();
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![
                NormalizedUnitInput {
                    raw_hash: RAW.to_owned(),
                    tool_profile_hash: TOOL.to_owned(),
                    r#gen: 0,
                    unit_key: "doc:1".to_owned(),
                    unit_content_hash: UNIT.to_owned(),
                    markdown: "# First\nfirst body of some length".to_owned(),
                },
                NormalizedUnitInput {
                    raw_hash: RAW.to_owned(),
                    tool_profile_hash: TOOL.to_owned(),
                    r#gen: 0,
                    unit_key: "doc:2".to_owned(),
                    unit_content_hash: UNIT.to_owned(),
                    markdown: "second body, no heading here".to_owned(),
                },
            ],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        let second_unit_row = rows
            .iter()
            .find(|row| row.unit_key == "doc:2")
            .expect("second unit must produce a chunk");
        // Restarted at 0, not continuing from the first unit's ~34-byte length.
        assert_eq!(second_unit_row.byte_start, 0);
        // No heading appears before it in doc:2, so heading_path must be empty —
        // not `["First"]` leaked across the unit boundary.
        assert_eq!(second_unit_row.heading_path, Some(Vec::new()));
        assert_eq!(second_unit_row.section_id, None);
    }

    // CT3-CHUNK-004 (gap fill): the existing ct3_chunk_004 test never exceeds
    // max_chars, so rule 5 (paragraph-boundary greedy split, single-paragraph-only
    // falls back to a hard character split; split pieces share heading_path /
    // section_id and are distinguished by unit-local span) was unverified.
    #[test]
    fn ct3_chunk_004_max_chars_splits_at_paragraph_boundary_and_shares_section() {
        let config = ChunkingConfig {
            chunking_config_hash: chunking_config_hash("heading", 20).unwrap(),
            strategy: "heading".to_owned(),
            max_chars: 20,
        };
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                // Section body ("paragraph one long enough\n\nparagraph two also
                // long enough") is well over max_chars=20 and has a paragraph
                // boundary (blank line) to split on.
                markdown: "# Heading\nparagraph one long enough\n\nparagraph two also long enough"
                    .to_owned(),
            }],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        let section_rows = rows
            .iter()
            .filter(|row| row.section_id.as_deref() == Some("heading"))
            .collect::<Vec<_>>();
        // The over-long section must be split into more than one chunk.
        assert!(
            section_rows.len() > 1,
            "expected max_chars to force a split, got {} chunk(s)",
            section_rows.len()
        );
        // Every span stays within max_chars (this fixture is pure ASCII, so byte
        // count and scalar count coincide — the multibyte fixtures below pin the
        // general byte-vs-scalar distinction), and split pieces share
        // heading_path / section_id but are distinguished by span.
        for row in &section_rows {
            let span = row.byte_end - row.byte_start;
            assert!(span <= 20, "split chunk exceeds max_chars: {span}");
            assert_eq!(row.heading_path, Some(vec!["Heading".to_owned()]));
        }
        let mut spans = section_rows
            .iter()
            .map(|row| (row.byte_start, row.byte_end))
            .collect::<Vec<_>>();
        spans.sort_unstable();
        // Contiguous, non-overlapping coverage (greedy split, no gaps beyond the
        // whitespace consumed between pieces).
        assert_eq!(spans[0].0, 0);
        assert!(spans.windows(2).all(|pair| pair[0].1 <= pair[1].0));
    }

    #[test]
    fn ct3_chunk_006_chunking_config_hash_vector() {
        assert_eq!(
            chunking_config_hash("heading", 6000).unwrap(),
            "sha256:7810328ffa7f0dd9a558294e166f20d8038d8d779809ee519582e3d6ba1b98ea"
        );
    }

    // U97: 03 §8.1/§8 fixes the persisted span as the UTF-8 BYTE offset into the
    // unit's markdown (unit-local, 0-based half-open) — the Unicode SCALAR (char)
    // index used above only to enforce max_chars (04 §4.1 rule 5). Every prior
    // test in this module is ASCII, where the two counts are numerically
    // identical and this distinction is invisible. These two tests pin the byte
    // semantics with multibyte (Japanese) fixtures where char count and byte
    // count provably diverge.
    #[test]
    fn u97_byte_span_is_utf8_byte_offset_not_scalar_count_at_section_boundary() {
        let config = default_chunking_config().unwrap();
        // "あ\n" is 2 Unicode scalars but 4 UTF-8 bytes (あ = U+3042 encodes to 3
        // bytes, \n to 1). The heading section "# 見出し\nbody" is 10 scalars but
        // 16 bytes (見/出/し are each 3 bytes).
        let markdown = "あ\n# 見出し\nbody".to_owned();
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: markdown.clone(),
            }],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        assert_eq!(rows.len(), 2);

        assert_eq!(rows[0].byte_start, 0);
        assert_eq!(rows[0].byte_end, 4);
        assert_eq!(rows[0].text, "あ\n");
        assert_eq!(rows[0].heading_path, Some(Vec::new()));

        assert_eq!(rows[1].byte_start, 4);
        assert_eq!(rows[1].byte_end, 20);
        assert_eq!(rows[1].text, "# 見出し\nbody");
        assert_eq!(rows[1].heading_path, Some(vec!["見出し".to_owned()]));
        // If the span were still scalar-indexed (the pre-U97 bug), byte_end -
        // byte_start would equal the 10-scalar count below, not the 16-byte one.
        assert_eq!(
            rows[1].byte_end - rows[1].byte_start,
            rows[1].text.len() as u64
        );
        assert_ne!(
            rows[1].byte_end - rows[1].byte_start,
            rows[1].text.chars().count() as u64
        );

        // Sanity: the whole unit is 20 UTF-8 bytes, matching the final chunk's
        // byte_end (no gap, no over/under-count at the unit tail).
        assert_eq!(markdown.len(), 20);
        assert_eq!(rows[1].byte_end, markdown.len() as u64);
    }

    #[test]
    fn u97_byte_span_recovers_exact_text_via_direct_byte_slice_across_hard_splits() {
        // A small max_chars over a paragraph-boundary-free CJK body forces
        // several hard splits (04 §4.1 rule 5) away from any section boundary —
        // Test 1 above only exercises one split, at a section edge. Every
        // resulting byte_start/byte_end must let a resolver recover `text` by
        // slicing the ORIGINAL unit markdown's UTF-8 bytes directly (08 §3.1
        // step 7 / 03 §8.1): a scalar-indexed span would slice the wrong bytes
        // (or panic mid-codepoint) for every interior chunk here, since every
        // character — heading kanji included — is a multi-byte CJK codepoint.
        let config = ChunkingConfig {
            chunking_config_hash: chunking_config_hash("heading", 5).unwrap(),
            strategy: "heading".to_owned(),
            max_chars: 5,
        };
        let body = "漢字".repeat(20);
        let markdown = format!("# 表題\n{body}");
        let input = ChunkingInput {
            raw_path: "a.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: markdown.clone(),
            }],
            config,
            created_at: "2026-07-03T00:00:00Z".to_owned(),
        };
        let rows = chunk_normalized_instance(input).unwrap();
        assert!(
            rows.len() > 5,
            "max_chars=5 over a {}-scalar CJK body must force several splits, got {} chunk(s)",
            body.chars().count(),
            rows.len()
        );
        for row in &rows {
            let start = row.byte_start as usize;
            let end = row.byte_end as usize;
            assert!(start < end);
            // Every chunk here mixes in or is entirely 3-byte CJK text, so its
            // byte span must be strictly wider than its scalar count — the
            // signature of a genuine byte offset (a mislabeled scalar offset
            // would make this equality hold instead).
            assert_ne!(end - start, row.text.chars().count());
            // The round trip Evidence Pointer resolution and fsck rely on:
            // slicing the ORIGINAL unit markdown's bytes at [byte_start,
            // byte_end) reproduces the chunk's exact stored text and hash.
            assert_eq!(&markdown[start..end], row.text.as_str());
            assert_eq!(row.text_hash, hash_bytes(row.text.as_bytes()));
        }
    }

    // V7 (W3): a chunk is a byte span of a unit (03 §8.1), so `max_chars` can cut
    // a Markdown image reference in half. `kio-search`'s extractor then drops the
    // fragment rather than guess a hash (fail-empty) — which silently costs the
    // Agent that image in `related_images[]` AND costs it its embedding, since
    // `referenced_image_hashes` reads the same chunk bodies. Nothing reports it.
    //
    // The plan left "how often" open. It is not a matter of luck: rule 5 splits at
    // the last blank line inside the window and falls back to a hard character cut
    // only when the window holds none, so a reference can be severed ONLY inside a
    // blank-line-free run longer than `max_chars`. These two pin both halves of
    // that at the shipped 6000 — a gallery with no blank line IS cut, and the very
    // same references with blank lines between them are not. That is what makes
    // the measurement over the real captures (`kio-adapter`'s
    // `no_real_page_holds_a_blank_line_free_run_that_a_chunk_boundary_could_cut`)
    // mean anything: it measures the one quantity this rule depends on.

    /// `lines` image references, joined by `separator` and nothing else. Each URI
    /// is a real 123-character one, so the char arithmetic here is the arithmetic
    /// the chunker actually does.
    fn image_gallery(lines: usize, separator: &str) -> String {
        (0..lines)
            .map(|index| {
                format!(
                    "![](kio://scope_01J8ZQ00000000000000000000/object/image/sha256:{index:064x})"
                )
            })
            .collect::<Vec<_>>()
            .join(separator)
    }

    /// True when the body's last image reference never closes — the signature of
    /// a span that ends mid-URI.
    ///
    /// Stated here rather than by calling `extract_related_images`: kio-index does
    /// not depend on kio-search, and the property being pinned is the chunker's,
    /// not the extractor's.
    fn ends_mid_reference(text: &str) -> bool {
        text.rfind("![](")
            .is_some_and(|start| !text[start..].contains(')'))
    }

    fn chunk_body(markdown: &str, config: ChunkingConfig) -> Vec<ChunkRow> {
        chunk_normalized_instance(ChunkingInput {
            raw_path: "gallery.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "page:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: markdown.to_owned(),
            }],
            config,
            created_at: "2026-08-06T00:00:00Z".to_owned(),
        })
        .expect("chunking must succeed")
    }

    #[test]
    fn v7_a_blank_line_free_run_lets_a_split_cut_an_image_reference() {
        let config = default_chunking_config().unwrap();
        assert_eq!(config.max_chars, 6000, "the shipped default this measures");
        // 129 characters per line (a 128-character reference plus its newline):
        // 46 lines fill 5934 of the first window, so the hard cut at 6000 lands 62
        // characters into the 47th URI, well inside its hash.
        let markdown = image_gallery(60, "\n");
        assert!(markdown.chars().count() > config.max_chars as usize);

        let rows = chunk_body(&markdown, config);
        assert!(rows.len() > 1, "the body is over max_chars, so it splits");
        assert!(
            rows.iter().any(|row| ends_mid_reference(&row.text)),
            "a gallery with no blank line must be cut inside a URI — if this stops \
             being true the rule the capture measurement rests on has changed"
        );
    }

    #[test]
    fn v7_the_same_references_separated_by_blank_lines_are_never_cut() {
        let config = default_chunking_config().unwrap();
        let markdown = image_gallery(60, "\n\n");
        let rows = chunk_body(&markdown, config);
        assert!(rows.len() > 1, "still well over max_chars");
        for row in &rows {
            assert!(
                !ends_mid_reference(&row.text),
                "a blank line inside the window is always preferred to a hard cut, \
                 so no span may end mid-URI: {:?}",
                row.text.chars().rev().take(40).collect::<String>()
            );
        }
    }

    fn time_chunk(n: usize) -> std::time::Duration {
        // A single large unit with no paragraph boundaries forces one hard split
        // every `max_chars`; the old `slice_chars` rescanned from the unit head on
        // every split (O(N²)). Small `max_chars` amplifies the split count so the
        // quadratic term dominates if present.
        let config = ChunkingConfig {
            chunking_config_hash: chunking_config_hash("heading", 200).unwrap(),
            strategy: "heading".to_owned(),
            max_chars: 200,
        };
        let input = ChunkingInput {
            raw_path: "big.md".to_owned(),
            units: vec![NormalizedUnitInput {
                raw_hash: RAW.to_owned(),
                tool_profile_hash: TOOL.to_owned(),
                r#gen: 0,
                unit_key: "doc:1".to_owned(),
                unit_content_hash: UNIT.to_owned(),
                markdown: "a".repeat(n),
            }],
            config,
            created_at: "2026-07-04T00:00:00Z".to_owned(),
        };
        let start = std::time::Instant::now();
        let rows = chunk_normalized_instance(input).expect("chunking must succeed");
        assert!(!rows.is_empty());
        start.elapsed()
    }

    // N6: chunking a single large document must be linear, not O(N²). Doubling the
    // input should ~double the time (~2x), not quadruple it (~4x, the bug). The
    // ratio is invariant to the debug/release constant factor; the absolute
    // backstop separates the fixed sub-second path from the old multi-second one.
    #[test]
    fn n6_chunking_scales_linearly_not_quadratically() {
        // Warm up (allocator/branch predictor) so the first measurement is not
        // penalized and the ratio stays honest.
        let _ = time_chunk(200_000);
        let t1 = time_chunk(1_000_000).min(time_chunk(1_000_000));
        let t2 = time_chunk(2_000_000).min(time_chunk(2_000_000));
        assert!(
            t2.as_secs_f64() < 10.0,
            "2MB chunking took {t2:?}; O(N²) slicing not eliminated"
        );
        assert!(
            t2.as_secs_f64() < t1.as_secs_f64() * 3.0 + 0.02,
            "chunking scaled super-linearly (O(N²)): 1MB={t1:?} 2MB={t2:?} (ratio {:.2})",
            t2.as_secs_f64() / t1.as_secs_f64().max(f64::MIN_POSITIVE)
        );
    }
}
