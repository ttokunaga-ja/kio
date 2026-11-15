//! Pure, deterministic bytes for the Rust persona fixture contract.
//!
//! The identities in this module are *planned* identities.  They deliberately
//! do not stand in for a Kio observed/attested hash.

use crate::persona_plan::{
    Disposition, FormatVariant, GateRole, PersonaId, PersonaPlan, TransformKind, source_projections,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const RENDERER_ID: &str = "kio-persona-renderer";
pub const RENDERER_SCHEMA: &str = "kio.persona.renderer/v2";
pub const MAX_RENDERED_BYTES: usize = 100 * 1024 * 1024;
pub const FROZEN_RENDER_VECTOR: &str =
    "sha256:ffbf3c2b96d557f7f3c42cbdcf7ecb741f34ff417dad3681b4ac131733831150";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("invalid render plan: {0}")]
    Invalid(String),
    #[error("invalid rendered source: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRenderPlan {
    pub persona_id: String,
    pub scope_id: String,
    pub source_id: String,
    pub version: u32,
    pub variant: FormatVariant,
    pub gate_role: GateRole,
    pub disposition: Disposition,
    /// Contract chunks, not adapter observations.  Only contributors have one.
    pub planned_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalMember {
    pub key: String,
    pub kind: UnitKind,
    pub ordinal: u32,
    pub planned_chunks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Document,
    Message,
    Page,
    Sheet,
    Slide,
    Image,
    Audio,
    Packet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSource {
    pub source_id: String,
    pub variant: FormatVariant,
    pub bytes: Vec<u8>,
    pub extension: &'static str,
    pub media_type: &'static str,
    pub logical_members: Vec<LogicalMember>,
    pub planned_chunks: u32,
    /// Hash of the plan and renderer schema; never an observed Kio hash.
    pub planned_identity: String,
    /// Renderer-local raw-byte digest, not Kio evidence or an attested hash.
    pub renderer_byte_digest: String,
}

/// Materialize a plan-owned structural transform.  No caller-selected source,
/// variant, or transform is accepted at this boundary.
pub fn render_structural(
    plan: &PersonaPlan,
    event_id: &str,
    parent: &RenderedSource,
    version: u32,
) -> Result<(RenderedSource, TransformWitness), RenderError> {
    plan.validate()
        .map_err(|error| RenderError::Invalid(format!("invalid persona plan: {error}")))?;
    render_structural_validated(plan, event_id, parent, version)
}

/// Plan-validated structural rendering for bounded suite construction.
pub(crate) fn render_structural_validated(
    plan: &PersonaPlan,
    event_id: &str,
    parent: &RenderedSource,
    version: u32,
) -> Result<(RenderedSource, TransformWitness), RenderError> {
    let mut found = None;
    for person in &plan.personas {
        if let Some(event) = person
            .structural
            .iter()
            .find(|event| event.event_id == event_id)
            && found.replace((person, event)).is_some()
        {
            return Err(RenderError::Invalid("ambiguous structural event".into()));
        }
    }
    let (person, event) =
        found.ok_or_else(|| RenderError::Invalid("structural event is not in plan".into()))?;
    let parent_id = event
        .parent_source_id
        .as_deref()
        .ok_or_else(|| RenderError::Invalid("structural event has no transform parent".into()))?;
    let expected_parent = source_projections(person)
        .map_err(|error| RenderError::Invalid(format!("source projection: {error}")))?
        .into_iter()
        .find(|source| source.source_id == parent_id)
        .ok_or_else(|| {
            RenderError::Invalid("structural parent is not a source projection".into())
        })?;
    let canonical_parent = render_canonical(&SourceRenderPlan {
        persona_id: person.id.as_str().into(),
        scope_id: expected_parent.scope_id,
        source_id: expected_parent.source_id,
        version,
        variant: expected_parent.variant,
        gate_role: expected_parent.gate_role,
        disposition: expected_parent.disposition,
        planned_chunks: expected_parent.planned_chunks,
    })?;
    if parent.source_id != parent_id
        || parent.variant != FormatVariant::Png
        || parent.renderer_byte_digest != digest(&parent.bytes)
        || *parent != canonical_parent
    {
        return Err(RenderError::Invalid(
            "structural parent does not match canonical PNG source".into(),
        ));
    }
    let variant = event
        .child_variant
        .ok_or_else(|| RenderError::Invalid("structural event has no child variant".into()))?;
    let (gate_role, disposition) = variant_policy(variant);
    let child = SourceRenderPlan {
        persona_id: person.id.as_str().into(),
        scope_id: event
            .destination_scope_id
            .clone()
            .or_else(|| event.source_scope_id.clone())
            .ok_or_else(|| RenderError::Invalid("structural event lacks scope".into()))?,
        source_id: event.source_id.clone(),
        version,
        variant,
        gate_role,
        disposition,
        planned_chunks: 0,
    };
    match event.transform {
        TransformKind::NearPngOneChannel => render_near_png(parent, &child),
        TransformKind::PngToScanPdf => render_scan_pdf_from_png(parent, &child),
        TransformKind::CanonicalSource => Err(RenderError::Invalid(
            "structural event is not a renderer transform".into(),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformWitness {
    pub transform: TransformKind,
    pub parent_planned_identity: String,
    pub child_planned_identity: String,
    pub changed_channel: Option<u8>,
    pub parent_renderer_byte_digest: String,
    pub child_renderer_byte_digest: String,
    pub parent_pixel_digest: String,
    pub parent_pixel_len: u8,
}

pub const ALL_VARIANTS: [FormatVariant; 22] = [
    FormatVariant::Md,
    FormatVariant::Markdown,
    FormatVariant::Txt,
    FormatVariant::Log,
    FormatVariant::Jsonl,
    FormatVariant::Json,
    FormatVariant::Yaml,
    FormatVariant::Xml,
    FormatVariant::Sql,
    FormatVariant::Csv,
    FormatVariant::Tsv,
    FormatVariant::Html,
    FormatVariant::Eml,
    FormatVariant::Ipynb,
    FormatVariant::PdfText,
    FormatVariant::PdfScan,
    FormatVariant::Docx,
    FormatVariant::Xlsx,
    FormatVariant::Pptx,
    FormatVariant::Png,
    FormatVariant::Wav,
    FormatVariant::Pcap,
];

pub fn variant_contract(variant: FormatVariant) -> (&'static str, &'static str) {
    match variant {
        FormatVariant::Md => ("md", "text/markdown"),
        FormatVariant::Markdown => ("markdown", "text/markdown"),
        FormatVariant::Txt => ("txt", "text/plain"),
        FormatVariant::Log
        | FormatVariant::Jsonl
        | FormatVariant::Json
        | FormatVariant::Yaml
        | FormatVariant::Xml
        | FormatVariant::Sql
        | FormatVariant::Csv
        | FormatVariant::Tsv
        | FormatVariant::Html
        | FormatVariant::Eml
        | FormatVariant::Ipynb => (
            match variant {
                FormatVariant::Log => "log",
                FormatVariant::Jsonl => "jsonl",
                FormatVariant::Json => "json",
                FormatVariant::Yaml => "yaml",
                FormatVariant::Xml => "xml",
                FormatVariant::Sql => "sql",
                FormatVariant::Csv => "csv",
                FormatVariant::Tsv => "tsv",
                FormatVariant::Html => "html",
                FormatVariant::Eml => "eml",
                FormatVariant::Ipynb => "ipynb",
                _ => unreachable!(),
            },
            "application/octet-stream",
        ),
        FormatVariant::PdfText | FormatVariant::PdfScan => ("pdf", "application/pdf"),
        FormatVariant::Docx => (
            "docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        FormatVariant::Xlsx => (
            "xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        FormatVariant::Pptx => (
            "pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        FormatVariant::Png => ("png", "image/png"),
        FormatVariant::Wav => ("wav", "audio/wav"),
        FormatVariant::Pcap => ("pcap", "application/vnd.tcpdump.pcap"),
    }
}

/// Render only an identity projected by the canonical persona plan.
pub fn render_person(
    plan: &PersonaPlan,
    person_id: PersonaId,
    version: u32,
) -> Result<Vec<RenderedSource>, RenderError> {
    plan.validate()
        .map_err(|error| RenderError::Invalid(format!("invalid persona plan: {error}")))?;
    render_person_validated(plan, person_id, version)
}

/// Render one person's projections after the caller has validated the complete
/// plan. This remains crate-private so input-facing boundaries cannot bypass
/// the canonical-plan check while suite construction avoids redundant checks.
pub(crate) fn render_person_validated(
    plan: &PersonaPlan,
    person_id: PersonaId,
    version: u32,
) -> Result<Vec<RenderedSource>, RenderError> {
    let person = plan
        .personas
        .iter()
        .find(|person| person.id == person_id)
        .ok_or_else(|| RenderError::Invalid("persona is not in plan".into()))?;
    source_projections(person)
        .map_err(|error| RenderError::Invalid(format!("source projection: {error}")))?
        .into_iter()
        .map(|source| {
            render_canonical(&SourceRenderPlan {
                persona_id: person.id.as_str().into(),
                scope_id: source.scope_id,
                source_id: source.source_id,
                version,
                variant: source.variant,
                gate_role: source.gate_role,
                disposition: source.disposition,
                planned_chunks: source.planned_chunks,
            })
        })
        .collect()
}

pub fn render_source(
    plan: &PersonaPlan,
    source_id: &str,
    version: u32,
) -> Result<RenderedSource, RenderError> {
    plan.validate()
        .map_err(|error| RenderError::Invalid(format!("invalid persona plan: {error}")))?;
    let mut matched = None;
    for person in &plan.personas {
        let projection = source_projections(person)
            .map_err(|error| RenderError::Invalid(format!("source projection: {error}")))?;
        if let Some(source) = projection
            .into_iter()
            .find(|source| source.source_id == source_id)
            && matched.replace((person, source)).is_some()
        {
            return Err(RenderError::Invalid("ambiguous source ID".into()));
        }
    }
    let (person, source) =
        matched.ok_or_else(|| RenderError::Invalid("source ID is not in persona plan".into()))?;
    render_canonical(&SourceRenderPlan {
        persona_id: person.id.as_str().into(),
        scope_id: source.scope_id,
        source_id: source.source_id,
        version,
        variant: source.variant,
        gate_role: source.gate_role,
        disposition: source.disposition,
        planned_chunks: source.planned_chunks,
    })
}

pub(crate) fn render_canonical(plan: &SourceRenderPlan) -> Result<RenderedSource, RenderError> {
    validate_plan(plan)?;
    let identity = planned_identity(plan);
    let body = render_body(plan, &identity)?;
    finish(plan, body, identity)
}

fn render_near_png(
    parent: &RenderedSource,
    child: &SourceRenderPlan,
) -> Result<(RenderedSource, TransformWitness), RenderError> {
    validate_plan(child)?;
    if child.variant != FormatVariant::Png
        || child.gate_role != GateRole::RawOnly
        || child.planned_chunks != 0
        || parent.renderer_byte_digest != digest(&parent.bytes)
        || parent.planned_identity == planned_identity(child)
    {
        return Err(RenderError::Invalid(
            "near transform child must be PNG".into(),
        ));
    }
    let mut pixels = decode_png(&parent.bytes)?;
    let channel = 1usize;
    pixels[channel] ^= 1;
    let identity = planned_identity(child);
    let rendered = finish(child, png(&pixels), identity)?;
    let witness = TransformWitness {
        transform: TransformKind::NearPngOneChannel,
        parent_planned_identity: parent.planned_identity.clone(),
        child_planned_identity: rendered.planned_identity.clone(),
        changed_channel: Some(channel as u8),
        parent_renderer_byte_digest: parent.renderer_byte_digest.clone(),
        child_renderer_byte_digest: rendered.renderer_byte_digest.clone(),
        parent_pixel_digest: digest(&decode_png(&parent.bytes)?),
        parent_pixel_len: 12,
    };
    validate_near_png(parent, &rendered, &witness)?;
    Ok((rendered, witness))
}

fn render_scan_pdf_from_png(
    parent: &RenderedSource,
    child: &SourceRenderPlan,
) -> Result<(RenderedSource, TransformWitness), RenderError> {
    validate_plan(child)?;
    if child.variant != FormatVariant::PdfScan
        || child.gate_role != GateRole::RawOnly
        || child.planned_chunks != 0
        || parent.renderer_byte_digest != digest(&parent.bytes)
        || parent.planned_identity == planned_identity(child)
    {
        return Err(RenderError::Invalid(
            "derive transform child must be scan PDF".into(),
        ));
    }
    let pixels = decode_png(&parent.bytes)?;
    let identity = planned_identity(child);
    let rendered = finish(child, scan_pdf(&pixels), identity)?;
    let witness = TransformWitness {
        transform: TransformKind::PngToScanPdf,
        parent_planned_identity: parent.planned_identity.clone(),
        child_planned_identity: rendered.planned_identity.clone(),
        changed_channel: None,
        parent_renderer_byte_digest: parent.renderer_byte_digest.clone(),
        child_renderer_byte_digest: rendered.renderer_byte_digest.clone(),
        parent_pixel_digest: digest(&pixels),
        parent_pixel_len: 12,
    };
    validate_scan_pdf_witness(parent, &rendered, &witness)?;
    Ok((rendered, witness))
}

fn validate_rendered(
    plan: &SourceRenderPlan,
    rendered: &RenderedSource,
) -> Result<(), RenderError> {
    validate_plan(plan)?;
    let (extension, media_type) = variant_contract(plan.variant);
    if rendered.extension != extension || rendered.media_type != media_type {
        return Err(RenderError::Malformed(
            "extension/media type do not match variant".into(),
        ));
    }
    if rendered.bytes.is_empty() || rendered.bytes.len() > MAX_RENDERED_BYTES {
        return Err(RenderError::Malformed(
            "bytes violate bounded nonempty contract".into(),
        ));
    }
    if rendered.planned_chunks != plan.planned_chunks {
        return Err(RenderError::Malformed("planned chunks changed".into()));
    }
    if rendered.planned_identity != planned_identity(plan) {
        return Err(RenderError::Malformed("planned identity mismatch".into()));
    }
    if rendered.renderer_byte_digest != digest(&rendered.bytes) {
        return Err(RenderError::Malformed("render digest mismatch".into()));
    }
    let member_chunks = rendered
        .logical_members
        .iter()
        .try_fold(0u32, |sum, member| sum.checked_add(member.planned_chunks))
        .ok_or_else(|| RenderError::Malformed("logical chunk arithmetic overflow".into()))?;
    if member_chunks != plan.planned_chunks {
        return Err(RenderError::Malformed(
            "logical chunk arithmetic mismatch".into(),
        ));
    }
    validate_members(plan, &rendered.logical_members)?;
    match plan.variant {
        FormatVariant::PdfText => {
            if !validate_pdf(&rendered.bytes, plan.planned_chunks.max(1), false) {
                return Err(RenderError::Malformed("text PDF lacks text layer".into()));
            }
        }
        FormatVariant::PdfScan => {
            if !validate_pdf(&rendered.bytes, 1, true) {
                return Err(RenderError::Malformed(
                    "scan PDF semantic form invalid".into(),
                ));
            }
        }
        FormatVariant::Png => {
            decode_png(&rendered.bytes)?;
        }
        FormatVariant::Wav => validate_wav(&rendered.bytes)?,
        FormatVariant::Pcap => validate_pcap(&rendered.bytes)?,
        FormatVariant::Docx | FormatVariant::Xlsx | FormatVariant::Pptx => {
            validate_ooxml(plan.variant, &rendered.bytes)?
        }
        _ => {
            let text = std::str::from_utf8(&rendered.bytes)
                .map_err(|_| RenderError::Malformed("text variant is not UTF-8".into()))?;
            match plan.variant {
                FormatVariant::Json => {
                    let value: serde_json::Value = serde_json::from_str(text)
                        .map_err(|_| RenderError::Malformed("invalid JSON".into()))?;
                    if value.get("id").and_then(|v| v.as_str()).is_none()
                        || value.get("source").and_then(|v| v.as_str())
                            != Some(plan.source_id.as_str())
                    {
                        return Err(RenderError::Malformed("JSON identity shape invalid".into()));
                    }
                }
                FormatVariant::Ipynb => {
                    let value: serde_json::Value = serde_json::from_str(text)
                        .map_err(|_| RenderError::Malformed("invalid notebook JSON".into()))?;
                    if value.get("nbformat").and_then(|v| v.as_u64()) != Some(4)
                        || value
                            .get("cells")
                            .and_then(|v| v.as_array())
                            .is_none_or(|cells| cells.len() != 1)
                    {
                        return Err(RenderError::Malformed("notebook shape invalid".into()));
                    }
                }
                FormatVariant::Jsonl => {
                    for line in text.lines() {
                        serde_json::from_str::<serde_json::Value>(line)
                            .map_err(|_| RenderError::Malformed("invalid JSONL".into()))?;
                    }
                }
                FormatVariant::Xml => {
                    if !text.starts_with("<?xml")
                        || !text.contains("</record>")
                        || !text.contains(&plan.source_id)
                    {
                        return Err(RenderError::Malformed("invalid XML shape".into()));
                    }
                }
                FormatVariant::Html => {
                    if !text.starts_with("<!doctype html>")
                        || !text.contains("</html>")
                        || !text.contains(&plan.source_id)
                    {
                        return Err(RenderError::Malformed("invalid HTML shape".into()));
                    }
                }
                FormatVariant::Eml => {
                    if !text.starts_with(
                        "From: fixture@example.invalid\nTo: kio@example.invalid\nSubject: ",
                    ) || !text.contains("\n\n")
                        || !text.contains(&plan.source_id)
                    {
                        return Err(RenderError::Malformed("invalid EML shape".into()));
                    }
                }
                FormatVariant::Csv => {
                    if text.lines().count() != 2 || !text.starts_with("source,id\n") {
                        return Err(RenderError::Malformed("invalid CSV shape".into()));
                    }
                }
                FormatVariant::Tsv
                    if text.lines().count() != 2 || !text.starts_with("source\tid\n") =>
                {
                    return Err(RenderError::Malformed("invalid TSV shape".into()));
                }
                FormatVariant::Yaml
                    if !text.starts_with("id: sha256:")
                        || !text.contains(&format!("source: {}\n", plan.source_id)) =>
                {
                    return Err(RenderError::Malformed("YAML identity shape invalid".into()));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_near_png(
    parent: &RenderedSource,
    child: &RenderedSource,
    witness: &TransformWitness,
) -> Result<(), RenderError> {
    if parent.renderer_byte_digest != digest(&parent.bytes)
        || child.renderer_byte_digest != digest(&child.bytes)
        || parent.planned_identity == child.planned_identity
        || parent.logical_members
            != vec![LogicalMember {
                key: "image:0".into(),
                kind: UnitKind::Image,
                ordinal: 0,
                planned_chunks: 0,
            }]
        || child.logical_members
            != vec![LogicalMember {
                key: "image:0".into(),
                kind: UnitKind::Image,
                ordinal: 0,
                planned_chunks: 0,
            }]
        || witness.transform != TransformKind::NearPngOneChannel
        || witness.changed_channel != Some(1)
        || witness.parent_planned_identity != parent.planned_identity
        || witness.child_planned_identity != child.planned_identity
        || witness.parent_renderer_byte_digest != parent.renderer_byte_digest
        || witness.child_renderer_byte_digest != child.renderer_byte_digest
    {
        return Err(RenderError::Malformed("invalid near-PNG witness".into()));
    }
    let a = decode_png(&parent.bytes)?;
    if witness.parent_pixel_len != 12 || witness.parent_pixel_digest != digest(&a) {
        return Err(RenderError::Malformed(
            "near-PNG pixel witness mismatch".into(),
        ));
    }
    let b = decode_png(&child.bytes)?;
    let changed: Vec<_> = a
        .iter()
        .zip(&b)
        .enumerate()
        .filter_map(|(i, (x, y))| (x != y).then_some(i))
        .collect();
    if changed != [1] {
        return Err(RenderError::Malformed(
            "near PNG must alter exactly the green channel of first pixel".into(),
        ));
    }
    Ok(())
}

fn validate_scan_pdf_witness(
    parent: &RenderedSource,
    child: &RenderedSource,
    witness: &TransformWitness,
) -> Result<(), RenderError> {
    if parent.renderer_byte_digest != digest(&parent.bytes)
        || child.renderer_byte_digest != digest(&child.bytes)
        || parent.planned_identity == child.planned_identity
        || parent.logical_members
            != vec![LogicalMember {
                key: "image:0".into(),
                kind: UnitKind::Image,
                ordinal: 0,
                planned_chunks: 0,
            }]
        || child.logical_members
            != vec![
                LogicalMember {
                    key: "page:1".into(),
                    kind: UnitKind::Page,
                    ordinal: 0,
                    planned_chunks: 0,
                },
                LogicalMember {
                    key: "image:0".into(),
                    kind: UnitKind::Image,
                    ordinal: 1,
                    planned_chunks: 0,
                },
            ]
        || witness.transform != TransformKind::PngToScanPdf
        || witness.parent_planned_identity != parent.planned_identity
        || witness.child_planned_identity != child.planned_identity
        || witness.parent_renderer_byte_digest != parent.renderer_byte_digest
        || witness.child_renderer_byte_digest != child.renderer_byte_digest
    {
        return Err(RenderError::Malformed("invalid PNG-to-PDF witness".into()));
    }
    let pixels = decode_png(&parent.bytes)?;
    if witness.parent_pixel_len != 12 || witness.parent_pixel_digest != digest(&pixels) {
        return Err(RenderError::Malformed(
            "scan PDF pixel witness mismatch".into(),
        ));
    }
    let parent_hex = hex(&pixels);
    let image = b"/Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /ASCIIHexDecode";
    let image_at = child
        .bytes
        .windows(image.len())
        .position(|part| part == image)
        .ok_or_else(|| RenderError::Malformed("scan PDF lacks image object".into()))?;
    let stream_at = child.bytes[image_at..]
        .windows(7)
        .position(|part| part == b"stream\n")
        .map(|at| image_at + at + 7)
        .ok_or_else(|| RenderError::Malformed("scan PDF image stream missing".into()))?;
    let stream_end = child.bytes[stream_at..]
        .windows(11)
        .position(|part| part == b">\nendstream")
        .map(|at| stream_at + at)
        .ok_or_else(|| RenderError::Malformed("scan PDF image stream terminator missing".into()))?;
    if &child.bytes[stream_at..stream_end] != parent_hex.as_bytes() {
        return Err(RenderError::Malformed(
            "scan PDF does not carry parent RGB witness".into(),
        ));
    }
    Ok(())
}

fn validate_plan(plan: &SourceRenderPlan) -> Result<(), RenderError> {
    if plan.persona_id.len() != 3
        || !plan.persona_id.starts_with('p')
        || !plan.persona_id[1..].bytes().all(|c| c.is_ascii_digit())
        || plan.scope_id.is_empty()
        || !plan
            .scope_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
        || plan.source_id.is_empty()
        || plan.source_id.len() != 14
        || !plan
            .source_id
            .starts_with(&format!("{}-src-", plan.persona_id))
        || !plan.source_id[8..].bytes().all(|b| b.is_ascii_digit())
        || plan.version > 999_999
    {
        return Err(RenderError::Invalid(
            "identity fields are not portable".into(),
        ));
    }
    let contributor = plan.gate_role == GateRole::ContractContributor;
    if (plan.gate_role, plan.disposition) != variant_policy(plan.variant) {
        return Err(RenderError::Invalid(
            "gate role/disposition do not match variant".into(),
        ));
    }
    if contributor != (plan.planned_chunks > 0) {
        return Err(RenderError::Invalid(
            "only contributors may have planned chunks".into(),
        ));
    }
    if plan.planned_chunks > 72 {
        return Err(RenderError::Invalid(
            "planned chunk bound exceeds 72".into(),
        ));
    }
    Ok(())
}

fn variant_policy(variant: FormatVariant) -> (GateRole, Disposition) {
    match variant {
        FormatVariant::Md | FormatVariant::Markdown | FormatVariant::Txt => {
            (GateRole::ContractContributor, Disposition::LocalText)
        }
        FormatVariant::PdfText => (GateRole::ContractContributor, Disposition::LocalPdfText),
        FormatVariant::Log
        | FormatVariant::Jsonl
        | FormatVariant::Json
        | FormatVariant::Yaml
        | FormatVariant::Xml
        | FormatVariant::Sql
        | FormatVariant::Csv
        | FormatVariant::Tsv
        | FormatVariant::Html
        | FormatVariant::Eml
        | FormatVariant::Ipynb => (GateRole::IncidentalSearchable, Disposition::IncidentalSniff),
        FormatVariant::PdfScan | FormatVariant::Png => {
            (GateRole::RawOnly, Disposition::AwaitingOcr)
        }
        FormatVariant::Docx | FormatVariant::Xlsx | FormatVariant::Pptx => {
            (GateRole::RawOnly, Disposition::AwaitConversion)
        }
        FormatVariant::Wav | FormatVariant::Pcap => {
            (GateRole::RawOnly, Disposition::UnsupportedBinary)
        }
    }
}

fn finish(
    plan: &SourceRenderPlan,
    bytes: Vec<u8>,
    identity: String,
) -> Result<RenderedSource, RenderError> {
    let (extension, media_type) = variant_contract(plan.variant);
    let members = members(plan);
    let rendered = RenderedSource {
        source_id: plan.source_id.clone(),
        variant: plan.variant,
        renderer_byte_digest: digest(&bytes),
        bytes,
        extension,
        media_type,
        logical_members: members,
        planned_chunks: plan.planned_chunks,
        planned_identity: identity,
    };
    validate_rendered(plan, &rendered)?;
    Ok(rendered)
}

fn members(plan: &SourceRenderPlan) -> Vec<LogicalMember> {
    let kind = match plan.variant {
        FormatVariant::PdfText | FormatVariant::PdfScan => UnitKind::Page,
        FormatVariant::Png => UnitKind::Image,
        FormatVariant::Wav => UnitKind::Audio,
        FormatVariant::Pcap => UnitKind::Packet,
        FormatVariant::Xlsx => UnitKind::Sheet,
        FormatVariant::Pptx => UnitKind::Slide,
        FormatVariant::Eml => UnitKind::Message,
        _ => UnitKind::Document,
    };
    let count = match plan.variant {
        FormatVariant::PdfText => plan.planned_chunks.max(1),
        FormatVariant::PdfScan => 2,
        _ => 1,
    };
    (0..count)
        .map(|ordinal| LogicalMember {
            key: member_key(plan.variant, ordinal),
            kind: if plan.variant == FormatVariant::PdfScan && ordinal == 1 {
                UnitKind::Image
            } else {
                kind
            },
            ordinal,
            planned_chunks: if plan.variant == FormatVariant::PdfText {
                1
            } else if ordinal == 0 {
                plan.planned_chunks
            } else {
                0
            },
        })
        .collect()
}

fn member_key(variant: FormatVariant, ordinal: u32) -> String {
    match variant {
        FormatVariant::PdfText => format!("page:{}", ordinal + 1),
        FormatVariant::PdfScan => {
            if ordinal == 0 {
                "page:1".into()
            } else {
                "image:0".into()
            }
        }
        FormatVariant::Xlsx => "sheet:fixture".into(),
        FormatVariant::Pptx => "slide:1".into(),
        FormatVariant::Png => "image:0".into(),
        FormatVariant::Wav => "audio:1".into(),
        FormatVariant::Pcap => "packet:1".into(),
        _ => "doc:1".into(),
    }
}
fn validate_members(plan: &SourceRenderPlan, members: &[LogicalMember]) -> Result<(), RenderError> {
    let count = match plan.variant {
        FormatVariant::PdfText => plan.planned_chunks.max(1),
        FormatVariant::PdfScan => 2,
        _ => 1,
    } as usize;
    if members.len() != count {
        return Err(RenderError::Malformed(
            "incorrect logical member count".into(),
        ));
    }
    for (i, m) in members.iter().enumerate() {
        let expected_kind = match plan.variant {
            FormatVariant::PdfText => UnitKind::Page,
            FormatVariant::PdfScan if i == 1 => UnitKind::Image,
            FormatVariant::PdfScan => UnitKind::Page,
            FormatVariant::Png => UnitKind::Image,
            FormatVariant::Wav => UnitKind::Audio,
            FormatVariant::Pcap => UnitKind::Packet,
            FormatVariant::Xlsx => UnitKind::Sheet,
            FormatVariant::Pptx => UnitKind::Slide,
            FormatVariant::Eml => UnitKind::Message,
            _ => UnitKind::Document,
        };
        let expected_chunks = if plan.variant == FormatVariant::PdfText {
            1
        } else if i == 0 {
            plan.planned_chunks
        } else {
            0
        };
        if m.ordinal != i as u32
            || m.key != member_key(plan.variant, i as u32)
            || m.kind != expected_kind
            || m.planned_chunks != expected_chunks
        {
            return Err(RenderError::Malformed(
                "logical member contract invalid".into(),
            ));
        }
    }
    Ok(())
}

fn render_body(plan: &SourceRenderPlan, id: &str) -> Result<Vec<u8>, RenderError> {
    let plain = format!(
        "Kio persona source={} scope={} version={} planned={}\n",
        plan.source_id, plan.scope_id, plan.version, id
    );
    Ok(match plan.variant {
        FormatVariant::Md | FormatVariant::Markdown => format!("# {}\n\n{}", plan.source_id, plain).into_bytes(),
        FormatVariant::Txt => plain.into_bytes(),
        FormatVariant::Log => format!("2026-07-13T00:00:00Z INFO {plain}").into_bytes(),
        FormatVariant::Jsonl => format!("{{\"source\":\"{}\",\"id\":\"{}\"}}\n", plan.source_id, id).into_bytes(),
        FormatVariant::Json => format!("{{\"id\":\"{}\",\"source\":\"{}\"}}\n", id, plan.source_id).into_bytes(),
        FormatVariant::Yaml => format!("id: {id}\nsource: {}\n", plan.source_id).into_bytes(),
        FormatVariant::Xml => format!("<?xml version=\"1.0\"?><record id=\"{id}\"><source>{}</source></record>\n", plan.source_id).into_bytes(),
        FormatVariant::Sql => format!("CREATE TABLE fixture(source TEXT);\nINSERT INTO fixture VALUES ('{}');\n", plan.source_id).into_bytes(),
        FormatVariant::Csv => format!("source,id\n{},{}\n", plan.source_id, id).into_bytes(),
        FormatVariant::Tsv => format!("source\tid\n{}\t{}\n", plan.source_id, id).into_bytes(),
        FormatVariant::Html => format!("<!doctype html><html><body><p>{plain}</p></body></html>\n").into_bytes(),
        FormatVariant::Eml => format!("From: fixture@example.invalid\nTo: kio@example.invalid\nSubject: {}\n\n{plain}", plan.source_id).into_bytes(),
        FormatVariant::Ipynb => format!("{{\"cells\":[{{\"cell_type\":\"markdown\",\"metadata\":{{}},\"source\":[\"{}\"]}}],\"metadata\":{{}},\"nbformat\":4,\"nbformat_minor\":5}}\n", id).into_bytes(),
        FormatVariant::PdfText => text_pdf(plan, id),
        FormatVariant::PdfScan => scan_pdf(&pixels_from(id)),
        FormatVariant::Docx => ooxml("docx", id), FormatVariant::Xlsx => ooxml("xlsx", id), FormatVariant::Pptx => ooxml("pptx", id),
        FormatVariant::Png => png(&pixels_from(id)), FormatVariant::Wav => wav(&pixels_from(id)), FormatVariant::Pcap => pcap(&pixels_from(id)),
    })
}

fn planned_identity(plan: &SourceRenderPlan) -> String {
    digest(
        format!(
            "{RENDERER_SCHEMA}|{}|{}|{}|{}|{:?}|{:?}|{:?}|{}",
            plan.persona_id,
            plan.scope_id,
            plan.source_id,
            plan.version,
            plan.variant,
            plan.gate_role,
            plan.disposition,
            plan.planned_chunks
        )
        .as_bytes(),
    )
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex_lower(&Sha256::digest(bytes)))
}
fn pixels_from(id: &str) -> Vec<u8> {
    Sha256::digest(id.as_bytes())[..12].to_vec()
}

fn text_pdf(plan: &SourceRenderPlan, id: &str) -> Vec<u8> {
    let pages = plan.planned_chunks.max(1);
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = String::new();
    for n in 0..pages {
        let page = 4 + n * 2;
        kids.push_str(&format!("{page} 0 R "));
        let text = format!("BT /F1 10 Tf 72 720 Td (Kio {id} page {}) Tj ET\n", n + 1);
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", page+1).into_bytes());
        objects
            .push(format!("<< /Length {} >>\nstream\n{}endstream", text.len(), text).into_bytes());
    }
    objects[1] = format!("<< /Type /Pages /Count {pages} /Kids [{kids}] >>").into_bytes();
    pdf(objects)
}
fn scan_pdf(p: &[u8]) -> Vec<u8> {
    let hex = hex(p);
    let content = b"q 72 0 0 72 72 648 cm /Im0 Do Q\n";
    pdf(vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(), b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>".to_vec(), format!("<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /ASCIIHexDecode /Length {} >>\nstream\n{}>\nendstream", hex.len()+2, hex).into_bytes(), format!("<< /Length {} >>\nstream\n{}endstream", content.len(), std::str::from_utf8(content).unwrap()).into_bytes()])
}
fn pdf(objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let x = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}
fn validate_pdf(bytes: &[u8], pages: u32, scan: bool) -> bool {
    // This is intentionally a parser for the bounded PDF subset emitted above,
    // rather than a permissive token scan.  All xref entries must point at the
    // corresponding object and every stream length is checked before semantic
    // references are accepted.
    if pages == 0
        || bytes.len() > MAX_RENDERED_BYTES
        || !bytes.starts_with(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")
    {
        return false;
    }
    let marker = b"startxref\n";
    let Some(marker_at) = bytes.windows(marker.len()).rposition(|part| part == marker) else {
        return false;
    };
    let suffix = &bytes[marker_at + marker.len()..];
    let Some(newline) = suffix.iter().position(|byte| *byte == b'\n') else {
        return false;
    };
    if &suffix[newline..] != b"\n%%EOF\n"
        || suffix[..newline].is_empty()
        || !suffix[..newline].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let Ok(xref_at) = std::str::from_utf8(&suffix[..newline])
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or(())
    else {
        return false;
    };
    if xref_at >= marker_at || bytes.get(xref_at..xref_at + 5) != Some(b"xref\n") {
        return false;
    }
    let mut at = xref_at + 5;
    let Some(header_end) = bytes[at..].iter().position(|byte| *byte == b'\n') else {
        return false;
    };
    let header = &bytes[at..at + header_end];
    at += header_end + 1;
    let Some((first, count)) = std::str::from_utf8(header)
        .ok()
        .and_then(|s| s.split_once(' '))
    else {
        return false;
    };
    let Ok(first) = first.parse::<usize>() else {
        return false;
    };
    let Ok(count) = count.parse::<usize>() else {
        return false;
    };
    let expected_count = if scan { 6 } else { pages as usize * 2 + 4 };
    if first != 0 || count != expected_count || count > 4096 {
        return false;
    }
    if bytes.get(at..at + 20) != Some(b"0000000000 65535 f \n") {
        return false;
    }
    at += 20;
    let mut offsets = Vec::with_capacity(count - 1);
    for _ in 1..count {
        let Some(entry) = bytes.get(at..at + 20) else {
            return false;
        };
        if &entry[10..] != b" 00000 n \n" || !entry[..10].iter().all(u8::is_ascii_digit) {
            return false;
        }
        let Ok(offset) = std::str::from_utf8(&entry[..10])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or(())
        else {
            return false;
        };
        if offset >= xref_at {
            return false;
        }
        offsets.push(offset);
        at += 20;
    }
    let trailer =
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n");
    if bytes.get(at..) != Some(trailer.as_bytes())
        || offsets.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return false;
    }
    let mut objects = Vec::with_capacity(count - 1);
    for (index, offset) in offsets.iter().enumerate() {
        let number = index + 1;
        let header = format!("{number} 0 obj\n");
        if bytes.get(*offset..offset + header.len()) != Some(header.as_bytes()) {
            return false;
        }
        let end = offsets.get(index + 1).copied().unwrap_or(xref_at);
        let body_start = offset + header.len();
        let Some(body) = bytes
            .get(body_start..end)
            .and_then(|value| value.strip_suffix(b"\nendobj\n"))
        else {
            return false;
        };
        objects.push(body);
    }
    if objects.first() != Some(&b"<< /Type /Catalog /Pages 2 0 R >>".as_slice()) {
        return false;
    }
    if scan {
        if objects.len() != 5
            || objects[1] != b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>"
            || objects[2] != b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
            || !valid_pdf_stream(objects[3], b"/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /ASCIIHexDecode")
            || !valid_pdf_stream(objects[4], b"")
        { return false; }
        let Some(image) = pdf_stream_data(objects[3]) else {
            return false;
        };
        let Some(content) = pdf_stream_data(objects[4]) else {
            return false;
        };
        return image.len() == 26
            && image.ends_with(b">\n")
            && image[..24]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_lowercase())
            && content == b"q 72 0 0 72 72 648 cm /Im0 Do Q\n";
    }
    if objects.len() != expected_count - 1
        || objects.get(1)
            != Some(
                &format!(
                    "<< /Type /Pages /Count {pages} /Kids [{}] >>",
                    (0..pages)
                        .map(|n| format!("{} 0 R ", 4 + n * 2))
                        .collect::<String>()
                )
                .into_bytes()
                .as_slice(),
            )
        || objects.get(2)
            != Some(&b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".as_slice())
    {
        return false;
    }
    for page in 0..pages as usize {
        let object = 3 + page * 2;
        let page_number = object + 1;
        let content_number = page_number + 1;
        let expected_page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_number} 0 R >>"
        );
        if objects[object] != expected_page.as_bytes()
            || !valid_pdf_stream(objects[object + 1], b"")
        {
            return false;
        }
        let Some(content) = pdf_stream_data(objects[object + 1]) else {
            return false;
        };
        if !content.starts_with(b"BT /F1 10 Tf 72 720 Td (Kio ")
            || !content.ends_with(format!(" page {}) Tj ET\n", page + 1).as_bytes())
        {
            return false;
        }
    }
    true
}

fn pdf_stream_data(object: &[u8]) -> Option<&[u8]> {
    let split_at = object
        .windows(b"\nstream\n".len())
        .position(|part| part == b"\nstream\n")?;
    let (dictionary, stream_with_marker) = object.split_at(split_at);
    let stream = &stream_with_marker[b"\nstream\n".len()..];
    let stream = stream.strip_suffix(b"endstream")?;
    let length_at = dictionary.windows(8).position(|part| part == b"/Length ")? + 8;
    let digits = &dictionary[length_at..];
    let length_end = digits.iter().position(|byte| !byte.is_ascii_digit())?;
    let length = std::str::from_utf8(&digits[..length_end])
        .ok()?
        .parse::<usize>()
        .ok()?;
    (stream.len() == length).then_some(stream)
}
fn valid_pdf_stream(object: &[u8], required_dictionary: &[u8]) -> bool {
    object.starts_with(b"<< ")
        && (required_dictionary.is_empty()
            || object
                .windows(required_dictionary.len())
                .any(|part| part == required_dictionary))
        && pdf_stream_data(object).is_some()
}

fn png(p: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(14);
    raw.push(0);
    raw.extend_from_slice(&p[..6]);
    raw.push(0);
    raw.extend_from_slice(&p[6..12]);
    let mut z = vec![0x78, 0x01];
    z.push(1);
    z.extend_from_slice(&(raw.len() as u16).to_le_bytes());
    z.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
    z.extend_from_slice(&raw);
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut c = Vec::from(kind.as_slice());
    c.extend_from_slice(data);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}
fn decode_png(b: &[u8]) -> Result<Vec<u8>, RenderError> {
    if b.len() < 45 || &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(RenderError::Malformed("not a PNG".into()));
    }
    let mut at = 8;
    let mut idat = Vec::new();
    let mut sequence = Vec::new();
    while at + 12 <= b.len() {
        let n = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        if at + 12 + n > b.len() {
            return Err(RenderError::Malformed("truncated PNG chunk".into()));
        }
        let k = &b[at + 4..at + 8];
        sequence.push(k.to_vec());
        let d = &b[at + 8..at + 8 + n];
        let got = u32::from_be_bytes(b[at + 8 + n..at + 12 + n].try_into().unwrap());
        let mut x = Vec::from(k);
        x.extend_from_slice(d);
        if crc32(&x) != got {
            return Err(RenderError::Malformed("PNG CRC mismatch".into()));
        }
        if k == b"IHDR" && d != [0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0] {
            return Err(RenderError::Malformed("PNG must be RGB 2x2".into()));
        }
        if k == b"IDAT" {
            idat.extend_from_slice(d)
        }
        if k == b"IEND" {
            if n != 0 || at + 12 != b.len() {
                return Err(RenderError::Malformed(
                    "PNG IEND/trailing layout invalid".into(),
                ));
            }
            break;
        }
        at += 12 + n;
    }
    if sequence != [b"IHDR".to_vec(), b"IDAT".to_vec(), b"IEND".to_vec()] {
        return Err(RenderError::Malformed(
            "PNG chunk order is not canonical".into(),
        ));
    }
    if idat.len() < 11 || idat[..2] != [0x78, 1] || idat[2] != 1 {
        return Err(RenderError::Malformed("unsupported PNG zlib form".into()));
    }
    let n = u16::from_le_bytes(idat[3..5].try_into().unwrap()) as usize;
    if n != 14
        || idat.len() != 2 + 1 + 2 + 2 + n + 4
        || u16::from_le_bytes(idat[5..7].try_into().unwrap()) != !(n as u16)
    {
        return Err(RenderError::Malformed("invalid PNG stored deflate".into()));
    }
    let raw = &idat[7..7 + n];
    if adler32(raw) != u32::from_be_bytes(idat[7 + n..11 + n].try_into().unwrap()) {
        return Err(RenderError::Malformed("PNG Adler mismatch".into()));
    }
    if raw[0] != 0 || raw[7] != 0 {
        return Err(RenderError::Malformed("PNG filter must be none".into()));
    }
    let mut p = Vec::new();
    p.extend_from_slice(&raw[1..7]);
    p.extend_from_slice(&raw[8..14]);
    Ok(p)
}
fn crc32(b: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &v in b {
        c ^= u32::from(v);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xedb8_8320
            } else {
                c >> 1
            }
        }
    }
    !c
}
fn adler32(b: &[u8]) -> u32 {
    let (mut a, mut d) = (1u32, 0u32);
    for &x in b {
        a = (a + u32::from(x)) % 65521;
        d = (d + a) % 65521
    }
    (d << 16) | a
}

fn wav(p: &[u8]) -> Vec<u8> {
    let mut d = Vec::new();
    for x in p.iter().cycle().take(160) {
        d.extend_from_slice(&(i16::from(*x) - 128).to_le_bytes())
    }
    let mut o = b"RIFF".to_vec();
    o.extend_from_slice(&(36 + d.len() as u32).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&8000u32.to_le_bytes());
    o.extend_from_slice(&16000u32.to_le_bytes());
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&(d.len() as u32).to_le_bytes());
    o.extend_from_slice(&d);
    o
}
fn validate_wav(b: &[u8]) -> Result<(), RenderError> {
    if b.len() != 364
        || &b[..4] != b"RIFF"
        || u32::from_le_bytes(b[4..8].try_into().unwrap()) != 356
        || &b[8..16] != b"WAVEfmt "
        || u16::from_le_bytes(b[20..22].try_into().unwrap()) != 1
        || u16::from_le_bytes(b[22..24].try_into().unwrap()) != 1
        || u32::from_le_bytes(b[24..28].try_into().unwrap()) != 8000
        || u32::from_le_bytes(b[28..32].try_into().unwrap()) != 16000
        || u16::from_le_bytes(b[32..34].try_into().unwrap()) != 2
        || u16::from_le_bytes(b[34..36].try_into().unwrap()) != 16
        || &b[36..40] != b"data"
        || u32::from_le_bytes(b[40..44].try_into().unwrap()) != 320
    {
        return Err(RenderError::Malformed(
            "WAV must be 8kHz mono 16-bit 160 frames".into(),
        ));
    }
    Ok(())
}
fn pcap(p: &[u8]) -> Vec<u8> {
    let mut o = vec![0xd4, 0xc3, 0xb2, 0xa1];
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&4u16.to_le_bytes());
    o.extend_from_slice(&0i32.to_le_bytes());
    o.extend_from_slice(&0u32.to_le_bytes());
    o.extend_from_slice(&65535u32.to_le_bytes());
    o.extend_from_slice(&1u32.to_le_bytes());
    o.extend_from_slice(&0u32.to_le_bytes());
    o.extend_from_slice(&0u32.to_le_bytes());
    let mut packet = vec![
        0x02, 0, 0, 0, 0, 1, 0x02, 0, 0, 0, 0, 2, 0x08, 0x00, 0x45, 0, 0, 28, 0, 0, 0x40, 0, 64,
        17, 0, 0, 192, 0, 2, p[0], 198, 51, 100, p[1], 0x27, 0x10, 0x4e, 0x20, 0, 8, 0, 0,
    ];
    let checksum = ipv4_checksum(&packet[14..34]);
    packet[24..26].copy_from_slice(&checksum.to_be_bytes());
    let udp = udp_checksum(&packet[26..30], &packet[30..34], &packet[34..42]);
    packet[40..42].copy_from_slice(&udp.to_be_bytes());
    o.extend_from_slice(&(packet.len() as u32).to_le_bytes());
    o.extend_from_slice(&(packet.len() as u32).to_le_bytes());
    o.extend_from_slice(&packet);
    o
}
fn validate_pcap(b: &[u8]) -> Result<(), RenderError> {
    if b.len() != 82
        || b[..4] != [0xd4, 0xc3, 0xb2, 0xa1]
        || u16::from_le_bytes(b[4..6].try_into().unwrap()) != 2
        || u16::from_le_bytes(b[6..8].try_into().unwrap()) != 4
        || u32::from_le_bytes(b[20..24].try_into().unwrap()) != 1
        || u32::from_le_bytes(b[32..36].try_into().unwrap()) != 42
        || u32::from_le_bytes(b[36..40].try_into().unwrap()) != 42
        || b[40..54] != [0x02, 0, 0, 0, 0, 1, 0x02, 0, 0, 0, 0, 2, 0x08, 0]
        || b[54] != 0x45
        || u16::from_be_bytes(b[56..58].try_into().unwrap()) != 28
        || b[63] != 17
        || ipv4_checksum(&b[54..74]) != 0
        || u16::from_be_bytes(b[78..80].try_into().unwrap()) != 8
        || u16::from_be_bytes(b[80..82].try_into().unwrap()) == 0
        || udp_checksum(&b[66..70], &b[70..74], &b[74..82]) != 0
    {
        return Err(RenderError::Malformed(
            "PCAP must be v2.4 with one 12 byte packet".into(),
        ));
    }
    Ok(())
}
fn udp_checksum(source: &[u8], destination: &[u8], datagram: &[u8]) -> u16 {
    let mut sum = 0u32;
    for part in [
        source,
        destination,
        &[0, 17, 0, datagram.len() as u8][..],
        datagram,
    ] {
        for pair in part.chunks(2) {
            sum += u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]));
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for word in header.as_chunks::<2>().0 {
        sum += u32::from(u16::from_be_bytes(*word));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn ooxml(kind: &str, id: &str) -> Vec<u8> {
    let root = match kind {
        "docx" => "word/document.xml",
        "xlsx" => "xl/workbook.xml",
        "pptx" => "ppt/presentation.xml",
        _ => unreachable!(),
    };
    let xml = |name: &str| {
        match name {
        "document" => format!("<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>{id}</w:t></w:r></w:p></w:body></w:document>").into_bytes(),
        "workbook" => b"<?xml version=\"1.0\"?><workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Fixture\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>".to_vec(),
        "sheet1" => format!("<?xml version=\"1.0\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>{id}</t></is></c></row></sheetData></worksheet>").into_bytes(),
        "styles" => b"<?xml version=\"1.0\"?><styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><fonts count=\"1\"/><fills count=\"1\"/><borders count=\"1\"/></styleSheet>".to_vec(),
        "presentation" => b"<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/></p:sldMasterIdLst></p:presentation>".to_vec(),
        "slide1" => format!("<?xml version=\"1.0\"?><p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"><p:cSld name=\"{id}\"/></p:sld>").into_bytes(),
        "layout1" => b"<?xml version=\"1.0\"?><p:sldLayout xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"/>".to_vec(),
        "master1" => b"<?xml version=\"1.0\"?><p:sldMaster xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"/>".to_vec(),
        "theme1" => b"<?xml version=\"1.0\"?><a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Kio\"/>".to_vec(),
        "core" => b"<?xml version=\"1.0\"?><cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\"/>".to_vec(),
        "app" => b"<?xml version=\"1.0\"?><Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"/>".to_vec(),
        _ => format!("<?xml version=\"1.0\"?><Properties>{id}:{name}</Properties>").into_bytes(),
    }
    };
    let content_types = content_types_body(kind);
    let mut parts = vec![
        (
            "[Content_Types].xml".to_owned(),
            format!("<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">{content_types}</Types>")
                .into_bytes(),
        ),
        ("_rels/.rels".to_owned(), relationship_document(&[("rId1", "officeDocument", root), ("rId2", "extended-properties", "docProps/app.xml"), ("rId3", "metadata/core-properties", "docProps/core.xml")])),
        ("docProps/core.xml".into(), xml("core")),
        ("docProps/app.xml".into(), xml("app")),
    ];
    match kind {
        "docx" => parts.extend([
            (String::from("word/document.xml"), xml("document")),
            (
                String::from("word/_rels/document.xml.rels"),
                relationship_document(&[]),
            ),
        ]),
        "xlsx" => parts.extend([
            (String::from("xl/workbook.xml"), xml("workbook")),
            (String::from("xl/worksheets/sheet1.xml"), xml("sheet1")),
            (String::from("xl/styles.xml"), xml("styles")),
            (
                String::from("xl/_rels/workbook.xml.rels"),
                relationship_document(&[
                    ("rId1", "worksheet", "worksheets/sheet1.xml"),
                    ("rId2", "styles", "styles.xml"),
                ]),
            ),
        ]),
        _ => parts.extend([
            (String::from("ppt/presentation.xml"), xml("presentation")),
            (String::from("ppt/slides/slide1.xml"), xml("slide1")),
            (String::from("ppt/theme/theme1.xml"), xml("theme1")),
            (
                String::from("ppt/slideLayouts/slideLayout1.xml"),
                xml("layout1"),
            ),
            (
                String::from("ppt/slideMasters/slideMaster1.xml"),
                xml("master1"),
            ),
            (
                String::from("ppt/_rels/presentation.xml.rels"),
                relationship_document(&[
                    ("rId1", "slide", "slides/slide1.xml"),
                    ("rId2", "slideMaster", "slideMasters/slideMaster1.xml"),
                ]),
            ),
            (
                String::from("ppt/slides/_rels/slide1.xml.rels"),
                relationship_document(&[(
                    "rId1",
                    "slideLayout",
                    "../slideLayouts/slideLayout1.xml",
                )]),
            ),
            (
                String::from("ppt/slideLayouts/_rels/slideLayout1.xml.rels"),
                relationship_document(&[(
                    "rId1",
                    "slideMaster",
                    "../slideMasters/slideMaster1.xml",
                )]),
            ),
            (
                String::from("ppt/slideMasters/_rels/slideMaster1.xml.rels"),
                relationship_document(&[
                    ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
                    ("rId2", "theme", "../theme/theme1.xml"),
                ]),
            ),
        ]),
    }
    zip(parts)
}
fn relationship_document(relationships: &[(&str, &str, &str)]) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{}</Relationships>",
        relationships.iter().map(|(id, relationship_type, target)| {
            let namespace = if *relationship_type == "metadata/core-properties" {
                "http://schemas.openxmlformats.org/package/2006/relationships"
            } else {
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
            };
            format!("<Relationship Id=\"{id}\" Type=\"{namespace}/{relationship_type}\" Target=\"{target}\"/>")
        }).collect::<String>()
    ).into_bytes()
}
fn content_types_body(kind: &str) -> &'static str {
    match kind {
        "docx" => {
            "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>"
        }
        "xlsx" => {
            "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>"
        }
        "pptx" => {
            "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/><Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/><Override PartName=\"/ppt/slides/slide1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/><Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/><Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/><Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>"
        }
        _ => unreachable!(),
    }
}
fn zip(mut p: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    p.sort_by(|a, b| a.0.cmp(&b.0));
    let mut o = Vec::new();
    let mut dirs = Vec::new();
    for (n, d) in &p {
        let at = o.len() as u32;
        let c = crc32(d);
        o.extend_from_slice(b"PK\x03\x04\x14\0\0\0\0\0\0\0!\0");
        o.extend_from_slice(&c.to_le_bytes());
        o.extend_from_slice(&(d.len() as u32).to_le_bytes());
        o.extend_from_slice(&(d.len() as u32).to_le_bytes());
        o.extend_from_slice(&(n.len() as u16).to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(n.as_bytes());
        o.extend_from_slice(d);
        dirs.push((n, c, d.len() as u32, at));
    }
    let central = o.len() as u32;
    for (n, c, l, at) in &dirs {
        o.extend_from_slice(b"PK\x01\x02\x14\0\x14\0\0\0\0\0\0\0!\0");
        o.extend_from_slice(&c.to_le_bytes());
        o.extend_from_slice(&l.to_le_bytes());
        o.extend_from_slice(&l.to_le_bytes());
        o.extend_from_slice(&(n.len() as u16).to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(&0u16.to_le_bytes());
        o.extend_from_slice(&0u32.to_le_bytes());
        o.extend_from_slice(&at.to_le_bytes());
        o.extend_from_slice(n.as_bytes());
    }
    let size = o.len() as u32 - central;
    o.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    o.extend_from_slice(&(dirs.len() as u16).to_le_bytes());
    o.extend_from_slice(&(dirs.len() as u16).to_le_bytes());
    o.extend_from_slice(&size.to_le_bytes());
    o.extend_from_slice(&central.to_le_bytes());
    o.extend_from_slice(&0u16.to_le_bytes());
    o
}
fn validate_ooxml(variant: FormatVariant, b: &[u8]) -> Result<(), RenderError> {
    if b.len() < 22
        || !b.starts_with(b"PK\x03\x04")
        || &b[b.len() - 22..b.len() - 18] != b"PK\x05\x06"
    {
        return Err(RenderError::Malformed(
            "OOXML must be a fixed stored ZIP".into(),
        ));
    }
    let eocd = b.len() - 22;
    if b[eocd + 4..eocd + 8].iter().any(|byte| *byte != 0)
        || b[eocd + 20..].iter().any(|byte| *byte != 0)
    {
        return Err(RenderError::Malformed(
            "ZIP disk/comment fields must be fixed".into(),
        ));
    }
    let total = u16::from_le_bytes(b[eocd + 10..eocd + 12].try_into().unwrap()) as usize;
    if u16::from_le_bytes(b[eocd + 8..eocd + 10].try_into().unwrap()) as usize != total {
        return Err(RenderError::Malformed(
            "ZIP central entry counts disagree".into(),
        ));
    }
    let central_size = u32::from_le_bytes(b[eocd + 12..eocd + 16].try_into().unwrap()) as usize;
    let central = u32::from_le_bytes(b[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    if central > eocd || central.checked_add(central_size) != Some(eocd) {
        return Err(RenderError::Malformed(
            "ZIP central directory is out of range".into(),
        ));
    }
    let mut at = 0usize;
    let mut members = BTreeMap::new();
    let mut locals = Vec::new();
    while at < central {
        if at + 30 > central || &b[at..at + 4] != b"PK\x03\x04" {
            return Err(RenderError::Malformed("bad ZIP local header".into()));
        }
        if u16::from_le_bytes(b[at + 6..at + 8].try_into().unwrap()) != 0
            || u16::from_le_bytes(b[at + 8..at + 10].try_into().unwrap()) != 0
            || b[at + 10..at + 14] != [0, 0, 0x21, 0]
        {
            return Err(RenderError::Malformed(
                "ZIP must be stored at the fixed epoch".into(),
            ));
        }
        let crc = u32::from_le_bytes(b[at + 14..at + 18].try_into().unwrap());
        let size = u32::from_le_bytes(b[at + 18..at + 22].try_into().unwrap()) as usize;
        let name_len = u16::from_le_bytes(b[at + 26..at + 28].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(b[at + 28..at + 30].try_into().unwrap()) as usize;
        let data_at = at
            .checked_add(30)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .ok_or_else(|| RenderError::Malformed("ZIP member offset overflow".into()))?;
        let data_end = data_at
            .checked_add(size)
            .ok_or_else(|| RenderError::Malformed("ZIP member size overflow".into()))?;
        if data_end > central {
            return Err(RenderError::Malformed("truncated ZIP member".into()));
        }
        let name = std::str::from_utf8(&b[at + 30..at + 30 + name_len])
            .map_err(|_| RenderError::Malformed("non-UTF8 ZIP member".into()))?;
        if crc32(&b[data_at..data_end]) != crc {
            return Err(RenderError::Malformed("ZIP member CRC mismatch".into()));
        }
        if members
            .insert(name.to_owned(), b[data_at..data_end].to_vec())
            .is_some()
        {
            return Err(RenderError::Malformed("duplicate OOXML member".into()));
        }
        locals.push((name.to_owned(), crc, size as u32, at as u32));
        at = data_at + size;
    }
    if at != central || total != locals.len() {
        return Err(RenderError::Malformed(
            "ZIP local count/central offset mismatch".into(),
        ));
    }
    let mut directory = central;
    for (expected_name, expected_crc, expected_size, expected_offset) in &locals {
        if directory + 46 > eocd || &b[directory..directory + 4] != b"PK\x01\x02" {
            return Err(RenderError::Malformed(
                "truncated ZIP central record".into(),
            ));
        }
        if b[directory + 4..directory + 16]
            .iter()
            .any(|byte| *byte != 0 && *byte != 0x14 && *byte != 0x21)
            || u16::from_le_bytes(b[directory + 10..directory + 12].try_into().unwrap()) != 0
            || u16::from_le_bytes(b[directory + 12..directory + 14].try_into().unwrap()) != 0
        {
            return Err(RenderError::Malformed(
                "ZIP central flags/method/time invalid".into(),
            ));
        }
        let crc = u32::from_le_bytes(b[directory + 16..directory + 20].try_into().unwrap());
        let size = u32::from_le_bytes(b[directory + 20..directory + 24].try_into().unwrap());
        if u32::from_le_bytes(b[directory + 24..directory + 28].try_into().unwrap()) != size {
            return Err(RenderError::Malformed(
                "ZIP central compressed size mismatch".into(),
            ));
        }
        let name_len =
            u16::from_le_bytes(b[directory + 28..directory + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(b[directory + 30..directory + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(b[directory + 32..directory + 34].try_into().unwrap()) as usize;
        if extra_len != 0
            || comment_len != 0
            || b[directory + 34..directory + 42]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(RenderError::Malformed(
                "ZIP central extra/comment/attributes invalid".into(),
            ));
        }
        let end = directory
            .checked_add(46)
            .and_then(|value| value.checked_add(name_len))
            .ok_or_else(|| RenderError::Malformed("ZIP central name overflow".into()))?;
        if end > eocd {
            return Err(RenderError::Malformed("truncated ZIP central name".into()));
        }
        let name = std::str::from_utf8(&b[directory + 46..end])
            .map_err(|_| RenderError::Malformed("non-UTF8 ZIP central name".into()))?;
        let offset = u32::from_le_bytes(b[directory + 42..directory + 46].try_into().unwrap());
        if name != expected_name
            || crc != *expected_crc
            || size != *expected_size
            || offset != *expected_offset
        {
            return Err(RenderError::Malformed(
                "ZIP central/local record mismatch".into(),
            ));
        }
        directory = end;
    }
    if directory != eocd {
        return Err(RenderError::Malformed("extra ZIP central record".into()));
    }
    let expected: &[&str] = match variant {
        FormatVariant::Docx => &[
            "[Content_Types].xml",
            "_rels/.rels",
            "docProps/app.xml",
            "docProps/core.xml",
            "word/_rels/document.xml.rels",
            "word/document.xml",
        ],
        FormatVariant::Xlsx => &[
            "[Content_Types].xml",
            "_rels/.rels",
            "docProps/app.xml",
            "docProps/core.xml",
            "xl/_rels/workbook.xml.rels",
            "xl/styles.xml",
            "xl/workbook.xml",
            "xl/worksheets/sheet1.xml",
        ],
        FormatVariant::Pptx => &[
            "[Content_Types].xml",
            "_rels/.rels",
            "docProps/app.xml",
            "docProps/core.xml",
            "ppt/_rels/presentation.xml.rels",
            "ppt/presentation.xml",
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/slides/_rels/slide1.xml.rels",
            "ppt/slides/slide1.xml",
            "ppt/theme/theme1.xml",
        ],
        _ => {
            return Err(RenderError::Malformed(
                "non-OOXML variant sent to OOXML validator".into(),
            ));
        }
    };
    if members.len() != expected.len() || !expected.iter().all(|name| members.contains_key(*name)) {
        return Err(RenderError::Malformed(
            "OOXML member inventory is not exact".into(),
        ));
    }
    let (kind, root, marker, expected_relationships) = match variant {
        FormatVariant::Docx => (
            "docx",
            "word/document.xml",
            "<w:document ",
            vec![
                (
                    "_rels/.rels",
                    vec![
                        ("rId1", "officeDocument", "word/document.xml"),
                        ("rId2", "extended-properties", "docProps/app.xml"),
                        ("rId3", "metadata/core-properties", "docProps/core.xml"),
                    ],
                ),
                ("word/_rels/document.xml.rels", vec![]),
            ],
        ),
        FormatVariant::Xlsx => (
            "xlsx",
            "xl/workbook.xml",
            "<workbook ",
            vec![
                (
                    "_rels/.rels",
                    vec![
                        ("rId1", "officeDocument", "xl/workbook.xml"),
                        ("rId2", "extended-properties", "docProps/app.xml"),
                        ("rId3", "metadata/core-properties", "docProps/core.xml"),
                    ],
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    vec![
                        ("rId1", "worksheet", "worksheets/sheet1.xml"),
                        ("rId2", "styles", "styles.xml"),
                    ],
                ),
            ],
        ),
        FormatVariant::Pptx => (
            "pptx",
            "ppt/presentation.xml",
            "<p:presentation ",
            vec![
                (
                    "_rels/.rels",
                    vec![
                        ("rId1", "officeDocument", "ppt/presentation.xml"),
                        ("rId2", "extended-properties", "docProps/app.xml"),
                        ("rId3", "metadata/core-properties", "docProps/core.xml"),
                    ],
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    vec![
                        ("rId1", "slide", "slides/slide1.xml"),
                        ("rId2", "slideMaster", "slideMasters/slideMaster1.xml"),
                    ],
                ),
                (
                    "ppt/slides/_rels/slide1.xml.rels",
                    vec![("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")],
                ),
                (
                    "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
                    vec![("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")],
                ),
                (
                    "ppt/slideMasters/_rels/slideMaster1.xml.rels",
                    vec![
                        ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
                        ("rId2", "theme", "../theme/theme1.xml"),
                    ],
                ),
            ],
        ),
        _ => unreachable!(),
    };
    let content_types = std::str::from_utf8(&members["[Content_Types].xml"])
        .map_err(|_| RenderError::Malformed("OOXML content types are not UTF-8".into()))?;
    let root_xml = std::str::from_utf8(&members[root])
        .map_err(|_| RenderError::Malformed("OOXML root XML is not UTF-8".into()))?;
    let expected_content_types = format!(
        "<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">{}</Types>",
        content_types_body(kind)
    );
    if content_types != expected_content_types || !root_xml.contains(marker) {
        return Err(RenderError::Malformed(
            "OOXML format-specific root is invalid".into(),
        ));
    }
    for (path, expected) in expected_relationships {
        if members.get(path).map(Vec::as_slice) != Some(relationship_document(&expected).as_slice())
        {
            return Err(RenderError::Malformed(
                "OOXML relationship types or targets are not canonical".into(),
            ));
        }
    }
    match variant {
        FormatVariant::Xlsx if !root_xml.contains("r:id=\"rId1\"") => {
            return Err(RenderError::Malformed(
                "workbook sheet does not bind to worksheet relationship".into(),
            ));
        }
        FormatVariant::Pptx
            if !root_xml.contains("<p:sldId id=\"256\" r:id=\"rId1\"/>")
                || !root_xml.contains("<p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/>") =>
        {
            return Err(RenderError::Malformed(
                "presentation does not bind slide/master relationships".into(),
            ));
        }
        _ => {}
    }
    for (name, data) in &members {
        if name.ends_with(".xml") || name.ends_with(".rels") {
            let text = std::str::from_utf8(data)
                .map_err(|_| RenderError::Malformed("OOXML XML is not UTF-8".into()))?;
            if !text.starts_with("<?xml") {
                return Err(RenderError::Malformed("OOXML XML lacks declaration".into()));
            }
        }
        if !name.ends_with(".rels") {
            continue;
        }
        let text = std::str::from_utf8(data)
            .map_err(|_| RenderError::Malformed("OOXML relationship XML is not UTF-8".into()))?;
        let mut targets = BTreeSet::new();
        for target in relationship_targets(text)? {
            if !targets.insert(target) {
                return Err(RenderError::Malformed(
                    "duplicate OOXML relationship target".into(),
                ));
            }
            let resolved = resolve_relationship(name, target)?;
            if !members.contains_key(&resolved) {
                return Err(RenderError::Malformed(format!(
                    "OOXML relationship target missing: {target}"
                )));
            }
        }
    }
    Ok(())
}
fn relationship_targets(xml: &str) -> Result<Vec<&str>, RenderError> {
    if !xml.starts_with("<?xml")
        || !xml.contains("<Relationships")
        || !xml.ends_with("</Relationships>")
    {
        return Err(RenderError::Malformed(
            "malformed OOXML relationships".into(),
        ));
    }
    let mut targets = Vec::new();
    let mut rest = xml;
    while let Some(index) = rest.find("Target=\"") {
        rest = &rest[index + 8..];
        let end = rest.find('"').ok_or_else(|| {
            RenderError::Malformed("unterminated OOXML relationship target".into())
        })?;
        let target = &rest[..end];
        if target.is_empty() || target.starts_with('/') || target.contains('\0') {
            return Err(RenderError::Malformed(
                "unsafe OOXML relationship target".into(),
            ));
        }
        targets.push(target);
        rest = &rest[end + 1..];
    }
    Ok(targets)
}
fn resolve_relationship(rels: &str, target: &str) -> Result<String, RenderError> {
    let source_dir = if rels == "_rels/.rels" {
        ""
    } else {
        let (dir, leaf) = rels
            .rsplit_once('/')
            .ok_or_else(|| RenderError::Malformed("invalid relationship member path".into()))?;
        let parent = dir
            .strip_suffix("/_rels")
            .ok_or_else(|| RenderError::Malformed("invalid relationship member path".into()))?;
        let _source = leaf
            .strip_suffix(".rels")
            .ok_or_else(|| RenderError::Malformed("invalid relationship member path".into()))?;
        parent
    };
    let mut parts: Vec<&str> = source_dir.split('/').filter(|x| !x.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(RenderError::Malformed(
                        "OOXML relationship escapes package".into(),
                    ));
                }
            }
            other if other.contains(':') => {
                return Err(RenderError::Malformed(
                    "unsafe OOXML relationship target".into(),
                ));
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(RenderError::Malformed(
            "empty OOXML relationship target".into(),
        ));
    }
    Ok(parts.join("/"))
}
fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        use std::fmt::Write as _;
        let _ = write!(s, "{x:02X}");
    }
    s
}
fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        use std::fmt::Write as _;
        let _ = write!(s, "{x:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(v: FormatVariant) -> SourceRenderPlan {
        let (gate_role, disposition) = variant_policy(v);
        SourceRenderPlan {
            persona_id: "p01".into(),
            scope_id: "primary-01".into(),
            source_id: "p01-src-000001".into(),
            version: 0,
            variant: v,
            gate_role,
            disposition,
            planned_chunks: if gate_role == GateRole::ContractContributor {
                2
            } else {
                0
            },
        }
    }
    #[test]
    fn every_variant_is_deterministic_and_valid() {
        for v in ALL_VARIANTS {
            let p = plan(v);
            let a = render_canonical(&p).unwrap();
            assert_eq!(a, render_canonical(&p).unwrap());
            validate_rendered(&p, &a).unwrap();
        }
    }
    #[test]
    fn plan_bound_person_rendering_covers_all_variants() {
        let fixture = crate::persona_plan::frozen_plan(crate::persona_plan::PersonaProfile::Tiny);
        let mut variants = Vec::new();
        for person in crate::persona_plan::PersonaId::ALL {
            for rendered in render_person(&fixture, person, 0).unwrap() {
                if !variants.contains(&rendered.variant) {
                    variants.push(rendered.variant);
                }
            }
        }
        assert_eq!(variants.len(), ALL_VARIANTS.len());
    }
    #[test]
    fn chunks_and_observed_identity_are_not_confused() {
        let p = plan(FormatVariant::Md);
        let a = render_canonical(&p).unwrap();
        assert_ne!(a.planned_identity, a.renderer_byte_digest);
        assert_eq!(
            a.logical_members
                .iter()
                .map(|m| m.planned_chunks)
                .sum::<u32>(),
            2
        );
    }
    #[test]
    fn png_witnesses_are_strict() {
        let parent = render_canonical(&plan(FormatVariant::Png)).unwrap();
        let mut n = plan(FormatVariant::Png);
        n.source_id = "p01-src-000002".into();
        let (near, w) = render_near_png(&parent, &n).unwrap();
        validate_near_png(&parent, &near, &w).unwrap();
        let mut d = plan(FormatVariant::PdfScan);
        d.source_id = "p01-src-000003".into();
        let (pdf, w) = render_scan_pdf_from_png(&parent, &d).unwrap();
        validate_scan_pdf_witness(&parent, &pdf, &w).unwrap();
    }
    #[test]
    fn rejects_malformed_png() {
        assert!(decode_png(b"bad").is_err());
    }
    #[test]
    fn validators_reject_mutations_without_panicking() {
        let mut png_bytes = png(&pixels_from("png"));
        png_bytes.push(0);
        assert!(decode_png(&png_bytes).is_err());
        let mut wave = wav(&pixels_from("wav"));
        wave[34] = 8;
        assert!(validate_wav(&wave).is_err());
        let mut capture = pcap(&pixels_from("pcap"));
        capture[79] = 7;
        assert!(validate_pcap(&capture).is_err());
        let mut bad_udp_checksum = pcap(&pixels_from("pcap"));
        bad_udp_checksum[81] ^= 1;
        assert!(validate_pcap(&bad_udp_checksum).is_err());
        let text_plan = plan(FormatVariant::PdfText);
        let pdf = text_pdf(&text_plan, "id");
        let mut broken_pdf = pdf.clone();
        broken_pdf[0] = b'!';
        assert!(!validate_pdf(&broken_pdf, 2, false));
        let xref = pdf.windows(5).position(|part| part == b"xref\n").unwrap();
        let mut forged_xref = pdf.clone();
        // The first in-use record must resolve to object 1, not merely contain
        // PDF-looking tokens elsewhere in the byte stream.
        forged_xref[xref + b"xref\n0 8\n0000000000 65535 f \n".len()] = b'9';
        assert!(!validate_pdf(&forged_xref, 2, false));
        let mut rendered = render_canonical(&plan(FormatVariant::Md)).unwrap();
        rendered.logical_members.push(LogicalMember {
            key: "doc:2".into(),
            kind: UnitKind::Document,
            ordinal: 1,
            planned_chunks: u32::MAX,
        });
        assert!(
            std::panic::catch_unwind(|| validate_rendered(&plan(FormatVariant::Md), &rendered))
                .unwrap()
                .is_err()
        );
    }
    fn docx_parts(root_target: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            ("[Content_Types].xml".into(), b"<?xml version=\"1.0\"?><Types/>".to_vec()),
            ("_rels/.rels".into(), format!("<?xml version=\"1.0\"?><Relationships><Relationship Target=\"{root_target}\"/></Relationships>").into_bytes()),
            ("docProps/app.xml".into(), b"<?xml version=\"1.0\"?><part/>".to_vec()),
            ("docProps/core.xml".into(), b"<?xml version=\"1.0\"?><part/>".to_vec()),
            ("word/_rels/document.xml.rels".into(), b"<?xml version=\"1.0\"?><Relationships></Relationships>".to_vec()),
            ("word/document.xml".into(), b"<?xml version=\"1.0\"?><part/>".to_vec()),
        ]
    }
    fn zip_members(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut at = 0;
        let mut parts = Vec::new();
        while bytes.get(at..at + 4) == Some(b"PK\x03\x04") {
            let size = u32::from_le_bytes(bytes[at + 18..at + 22].try_into().unwrap()) as usize;
            let name_len = u16::from_le_bytes(bytes[at + 26..at + 28].try_into().unwrap()) as usize;
            let data_at = at + 30 + name_len;
            parts.push((
                String::from_utf8(bytes[at + 30..data_at].to_vec()).unwrap(),
                bytes[data_at..data_at + size].to_vec(),
            ));
            at = data_at + size;
        }
        parts
    }
    #[test]
    fn ooxml_inventory_and_relationships_fail_closed() {
        for (variant, kind, count) in [
            (FormatVariant::Docx, "docx", 6),
            (FormatVariant::Xlsx, "xlsx", 8),
            (FormatVariant::Pptx, "pptx", 13),
        ] {
            let bytes = ooxml(kind, "fixture");
            validate_ooxml(variant, &bytes).unwrap();
            let mut at = 0;
            let mut members = 0;
            while at + 4 <= bytes.len() && &bytes[at..at + 4] == b"PK\x03\x04" {
                let name = u16::from_le_bytes(bytes[at + 26..at + 28].try_into().unwrap()) as usize;
                let size = u32::from_le_bytes(bytes[at + 18..at + 22].try_into().unwrap()) as usize;
                members += 1;
                at += 30 + name + size;
            }
            assert_eq!(members, count);
        }
        let valid = ooxml("docx", "fixture");
        let central =
            u32::from_le_bytes(valid[valid.len() - 6..valid.len() - 2].try_into().unwrap())
                as usize;
        let mut bad_name = valid.clone();
        bad_name[central + 46] ^= 1;
        assert!(validate_ooxml(FormatVariant::Docx, &bad_name).is_err());
        let mut bad_crc = valid.clone();
        bad_crc[central + 16] ^= 1;
        assert!(validate_ooxml(FormatVariant::Docx, &bad_crc).is_err());
        let mut bad_offset = valid.clone();
        bad_offset[central + 42] ^= 1;
        assert!(validate_ooxml(FormatVariant::Docx, &bad_offset).is_err());
        let mut bad_count = valid;
        let count_at = bad_count.len() - 12;
        bad_count[count_at] ^= 1;
        assert!(validate_ooxml(FormatVariant::Docx, &bad_count).is_err());
        assert!(validate_ooxml(FormatVariant::Docx, &zip(docx_parts("missing.xml"))).is_err());
        assert!(validate_ooxml(FormatVariant::Docx, &zip(docx_parts("../../escape.xml"))).is_err());
        let mut missing = docx_parts("word/document.xml");
        missing.pop();
        assert!(validate_ooxml(FormatVariant::Docx, &zip(missing)).is_err());
        let mut duplicate = docx_parts("word/document.xml");
        duplicate.push((
            "word/document.xml".into(),
            b"<?xml version=\"1.0\"?><part/>".to_vec(),
        ));
        assert!(validate_ooxml(FormatVariant::Docx, &zip(duplicate)).is_err());
        let mut duplicate_target = docx_parts("word/document.xml");
        duplicate_target[1].1 = b"<?xml version=\"1.0\"?><Relationships><Relationship Target=\"word/document.xml\"/><Relationship Target=\"word/document.xml\"/></Relationships>".to_vec();
        assert!(validate_ooxml(FormatVariant::Docx, &zip(duplicate_target)).is_err());
        assert!(
            relationship_targets(
                "<?xml version=\"1.0\"?><Relationships><Relationship Target=\"oops></Relationships>"
            )
            .is_err()
        );
        let mut workbook = zip_members(&ooxml("xlsx", "fixture"));
        let rels = workbook
            .iter_mut()
            .find(|(name, _)| name == "xl/_rels/workbook.xml.rels")
            .unwrap();
        rels.1 = relationship_document(&[
            ("rId1", "styles", "worksheets/sheet1.xml"),
            ("rId2", "styles", "styles.xml"),
        ]);
        assert!(validate_ooxml(FormatVariant::Xlsx, &zip(workbook)).is_err());
        let mut content_types = zip_members(&ooxml("pptx", "fixture"));
        let types = content_types
            .iter_mut()
            .find(|(name, _)| name == "[Content_Types].xml")
            .unwrap();
        types.1 = b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"/>".to_vec();
        assert!(validate_ooxml(FormatVariant::Pptx, &zip(content_types)).is_err());
        let root_rels =
            relationship_document(&[("rId3", "metadata/core-properties", "docProps/core.xml")]);
        assert!(std::str::from_utf8(&root_rels).unwrap().contains(
            "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties"
        ));
    }
    #[test]
    fn source_code_files_are_not_searchable_fixture_variants() {
        for variant in ALL_VARIANTS {
            let (extension, _) = variant_contract(variant);
            assert!(!["rs", "py", "js", "ts", "go", "java", "c", "h", "cpp"].contains(&extension));
        }
    }

    #[test]
    fn frozen_contract_vector() {
        let mut material = Vec::new();
        for variant in ALL_VARIANTS {
            material.extend_from_slice(
                render_canonical(&plan(variant))
                    .unwrap()
                    .renderer_byte_digest
                    .as_bytes(),
            );
            material.push(b'\n');
        }
        let parent = render_canonical(&plan(FormatVariant::Png)).unwrap();
        let mut near_plan = plan(FormatVariant::Png);
        near_plan.source_id = "p01-src-000002".into();
        let (near, near_witness) = render_near_png(&parent, &near_plan).unwrap();
        let mut scan_plan = plan(FormatVariant::PdfScan);
        scan_plan.source_id = "p01-src-000003".into();
        let (scan, scan_witness) = render_scan_pdf_from_png(&parent, &scan_plan).unwrap();
        for value in [
            &near.renderer_byte_digest,
            &scan.renderer_byte_digest,
            &near_witness.parent_pixel_digest,
            &scan_witness.parent_pixel_digest,
        ] {
            material.extend_from_slice(value.as_bytes());
            material.push(b'\n');
        }
        assert_eq!(digest(&material), FROZEN_RENDER_VECTOR);
    }
}
