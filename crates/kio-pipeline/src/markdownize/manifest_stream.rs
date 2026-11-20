//! Bounded encoding of normalized manifest metadata. Canonical members have
//! fixed ASCII names and are emitted in JCS key order, so the serializer can
//! stream arrays without a document-sized sort buffer.

use std::io::{self, Write};

use kio_core::error::{PageExtractionRetry, Resource, ResourceLimit, ResourceTier};
use serde::Serialize;
use serde::ser::{SerializeSeq, SerializeStruct};
use sha2::{Digest, Sha256};

use super::{
    MAX_NORMALIZED_MANIFEST_BYTES, NormalizedInstanceManifest, NormalizedUnitManifestEntry,
};
use crate::{PipelineError, Result};

struct CanonicalFormatter;

impl serde_json::ser::Formatter for CanonicalFormatter {
    fn write_u64<W: Write + ?Sized>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        if value <= (1 << 53) {
            return serde_json::ser::CompactFormatter.write_u64(writer, value);
        }
        // JCS formats JSON numbers with IEEE 754 semantics. Delegate the
        // exceptional integer to the same encoder used for other CAS objects.
        serde_jcs::to_writer(writer, &value).map_err(io::Error::other)
    }
}

struct Canonical<'a>(&'a NormalizedInstanceManifest);

impl Serialize for Canonical<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let manifest = self.0;
        let mut out = serializer.serialize_struct("NormalizedInstanceManifest", 8)?;
        out.serialize_field("gen", &manifest.r#gen)?;
        out.serialize_field("generated_at", &manifest.generated_at)?;
        out.serialize_field("parent_gen", &manifest.parent_gen)?;
        let provenance = &manifest.preparation_provenance;
        out.serialize_field(
            "preparation_provenance",
            &CanonicalProvenance {
                producer_arch: &provenance.producer_arch,
                producer_os: &provenance.producer_os,
                renderer_name: provenance.renderer_name.as_deref(),
                renderer_version: provenance.renderer_version.as_deref(),
            },
        )?;
        out.serialize_field("raw_hash", &manifest.raw_hash)?;
        out.serialize_field("run_id", &manifest.run_id)?;
        out.serialize_field("tool_profile_hash", &manifest.tool_profile_hash)?;
        out.serialize_field("units", &Units(&manifest.units))?;
        out.end()
    }
}

#[derive(Serialize)]
struct CanonicalProvenance<'a> {
    producer_arch: &'a str,
    producer_os: &'a str,
    renderer_name: Option<&'a str>,
    renderer_version: Option<&'a str>,
}

struct Units<'a>(&'a [NormalizedUnitManifestEntry]);

impl Serialize for Units<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for unit in self.0 {
            sequence.serialize_element(&Unit(unit))?;
        }
        sequence.end()
    }
}

struct Unit<'a>(&'a NormalizedUnitManifestEntry);

impl Serialize for Unit<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let unit = self.0;
        let mut out = serializer.serialize_struct(
            "NormalizedUnitManifestEntry",
            9 + usize::from(unit.resource_limit.is_some()),
        )?;
        out.serialize_field("error_kind", &unit.error_kind)?;
        out.serialize_field("order", &unit.order)?;
        out.serialize_field("preparation_profile_hash", &unit.preparation_profile_hash)?;
        out.serialize_field("prepared_hash", &unit.prepared_hash)?;
        if let Some(limit) = &unit.resource_limit {
            out.serialize_field(
                "resource_limit",
                &CanonicalLimit {
                    limit: limit.limit,
                    observed: limit.observed,
                    page_progress: limit.page_progress.as_deref().map(|progress| {
                        CanonicalPageProgress {
                            extracted_through: progress.extracted_through,
                            next_page: progress.next_page,
                            retry: progress.retry,
                            staged_through: progress.staged_through,
                            total_pages: progress.total_pages,
                        }
                    }),
                    resource: limit.resource,
                    resource_tier: limit.resource_tier,
                    stage: &limit.stage,
                },
            )?;
        }
        out.serialize_field("status", &unit.status)?;
        out.serialize_field("unit_key", &unit.unit_key)?;
        out.serialize_field("unit_object_hash", &unit.unit_object_hash)?;
        out.serialize_field("unit_ref", &unit.unit_ref)?;
        out.serialize_field("unit_type", &unit.unit_type)?;
        out.end()
    }
}

#[derive(Serialize)]
struct CanonicalLimit<'a> {
    limit: Option<u64>,
    observed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_progress: Option<CanonicalPageProgress>,
    resource: Resource,
    resource_tier: Option<ResourceTier>,
    stage: &'a str,
}

#[derive(Serialize)]
struct CanonicalPageProgress {
    extracted_through: u64,
    next_page: Option<u64>,
    retry: PageExtractionRetry,
    #[serde(skip_serializing_if = "Option::is_none")]
    staged_through: Option<u64>,
    total_pages: u64,
}

#[derive(Clone, Copy)]
pub(super) enum Encoding {
    Pretty,
    Canonical,
}

/// Write only admitted bytes and optionally compute their content address.
/// Counting-only callers pass an I/O sink and leave hashing disabled.
pub(super) struct Bounded<W> {
    output: W,
    limit: u64,
    count: u64,
    exceeded: bool,
    hash: Option<Sha256>,
}

impl<W: Write> Bounded<W> {
    pub fn new(output: W, limit: u64, hash: bool) -> Self {
        Self {
            output,
            limit,
            count: 0,
            exceeded: false,
            hash: hash.then(Sha256::new),
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.count
    }

    pub fn limit_failure(&self) -> Option<PipelineError> {
        self.exceeded.then(|| {
            PipelineError::ResourceLimit(ResourceLimit {
                resource: Resource::Memory,
                stage: "normalized_manifest".to_owned(),
                limit: Some(self.limit),
                observed: self.limit.checked_add(1),
                resource_tier: None,
                page_progress: None,
            })
        })
    }

    pub fn check(&self, result: serde_json::Result<()>) -> Result<()> {
        if let Some(error) = self.limit_failure() {
            return Err(error);
        }
        result.map_err(|error| PipelineError::Schema(error.to_string()))
    }

    pub fn hash(self) -> String {
        let digest = self.hash.expect("hashing enabled").finalize();
        format!("sha256:{}", kio_core::cas::lower_hex(&digest))
    }
}

impl<W: Write> Write for Bounded<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit - self.count {
            self.exceeded = true;
            return Err(io::Error::other(
                "normalized manifest exceeds its byte limit",
            ));
        }
        self.output.write_all(bytes)?;
        self.count += bytes.len() as u64;
        if let Some(hash) = &mut self.hash {
            hash.update(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

pub(super) fn encode(
    writer: impl Write,
    manifest: &NormalizedInstanceManifest,
    encoding: Encoding,
) -> serde_json::Result<()> {
    match encoding {
        Encoding::Pretty => serde_json::to_writer_pretty(writer, manifest),
        Encoding::Canonical => Canonical(manifest).serialize(
            &mut serde_json::Serializer::with_formatter(writer, CanonicalFormatter),
        ),
    }
}

pub(super) fn admit(manifest: &NormalizedInstanceManifest) -> Result<()> {
    let mut writer = Bounded::new(io::sink(), MAX_NORMALIZED_MANIFEST_BYTES, false);
    let result = encode(&mut writer, manifest, Encoding::Pretty);
    writer.check(result)
}
