//! Typed, plan-bound ledgers for the Rust-owned persona fixture.
//!
//! A manifest describes material that *is planned to be written*.  It is not
//! evidence that a Kio instance has indexed it, nor does it make any claim
//! about replay or history readiness.

use std::collections::{BTreeMap, BTreeSet};

use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::persona_plan::{
    Disposition, FormatVariant, GateRole, PersonaId, PersonaPlan, PersonaPlanError,
    source_projections,
};
use crate::persona_render::{RenderedSource, UnitKind, render_person_validated, variant_contract};

pub const SCHEMA: &str = "kio.persona.manifest/v2";
pub const MAX_RAW_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_SCOPE_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SCOPE_MANIFEST_ROWS: usize = 1_000_000;
pub const MAX_SCOPE_MANIFEST_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_SUITE_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SUITE_SUMMARIES: usize = 400;
pub const MAX_SUMMARY_IDENTITIES: usize = 100_000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PersonaManifestError {
    #[error("plan: {0}")]
    Plan(String),
    #[error("serialization: {0}")]
    Serialize(String),
    #[error("JSON: {0}")]
    Json(String),
    #[error("noncanonical JCS JSONL")]
    NonCanonical,
    #[error("invalid persona manifest: {0}")]
    Invalid(String),
}

impl From<PersonaPlanError> for PersonaManifestError {
    fn from(value: PersonaPlanError) -> Self {
        Self::Plan(value.to_string())
    }
}

/// Metadata supplied by the deterministic renderer for one planned source.
/// The bytes themselves remain in the fixture artifact, not in its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalRawInput {
    pub source_id: String,
    pub raw_sha256: String,
    pub byte_len: u64,
    pub logical_members: Vec<LogicalMemberInput>,
}

/// Renderer-owned logical units projected from a single physical source.
/// `ordinal` is contiguous within that source; a parent can only name an
/// earlier ordinal from the same source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalMemberInput {
    pub kind: UnitKind,
    /// Renderer-local key; the manifest prefixes it with the source identity.
    pub key: String,
    pub ordinal: u32,
    pub planned_chunks: u32,
    pub parent_ordinal: Option<u32>,
}

impl PhysicalRawInput {
    /// Lossless adapter from the canonical renderer metadata.  Placement and
    /// plan marginals are intentionally derived by `ScopeManifest`, not copied
    /// from this renderer-local result.
    #[must_use]
    pub fn from_rendered(rendered: &RenderedSource) -> Self {
        Self {
            source_id: rendered.source_id.clone(),
            raw_sha256: rendered.renderer_byte_digest.clone(),
            byte_len: rendered.bytes.len() as u64,
            logical_members: rendered
                .logical_members
                .iter()
                .map(|member| LogicalMemberInput {
                    kind: member.kind,
                    key: member.key.clone(),
                    ordinal: member.ordinal,
                    planned_chunks: member.planned_chunks,
                    parent_ordinal: None,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalRaw {
    pub person_id: PersonaId,
    pub scope_id: String,
    pub source_id: String,
    pub variant: FormatVariant,
    pub gate_role: GateRole,
    pub disposition: Disposition,
    pub raw_sha256: String,
    pub byte_len: u64,
    pub extension: String,
    pub media_type: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalItem {
    pub logical_id: String,
    pub source_id: String,
    pub parent_logical_id: Option<String>,
    pub contiguous_index: u32,
    pub source_ordinal: u32,
    pub unit_kind: UnitKind,
    pub unit_key: String,
    pub person_id: PersonaId,
    pub scope_id: String,
    pub variant: FormatVariant,
    pub planned_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchableExpectation {
    pub unit_key: String,
    pub source_id: String,
    pub person_id: PersonaId,
    pub scope_id: String,
    pub variant: FormatVariant,
    pub gate_role: GateRole,
    pub disposition: Disposition,
    pub raw_sha256: String,
    pub path: String,
    pub planned_chunks: u32,
    pub planned_unit_keys: Vec<String>,
}

/// Three independent ledgers.  They are compact: every row references a
/// person/scope/source identity from the plan rather than embedding the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeManifest {
    pub schema: String,
    pub plan_sha256: String,
    pub person_id: PersonaId,
    pub scope_id: String,
    pub physical_raw: Vec<PhysicalRaw>,
    pub logical_items: Vec<LogicalItem>,
    pub searchable_expectations: Vec<SearchableExpectation>,
    pub state_root: String,
    pub semantic_root: String,
}

/// Compact identity projection retained by the suite ledger solely to reject
/// duplicate inventory across shard boundaries.  It deliberately has no
/// logical content, bytes, or duplicated plan fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityProjection {
    pub source_id: String,
    pub raw_sha256: String,
    pub path_casefolded: String,
    pub logical_count: u32,
    pub logical_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeManifestSummary {
    pub person_id: PersonaId,
    pub scope_id: String,
    pub shard_sha256: String,
    pub byte_len: u64,
    pub physical_rows: u32,
    pub logical_rows: u32,
    pub state_root: String,
    pub semantic_root: String,
    pub identities: Vec<IdentityProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteManifest {
    pub schema: String,
    pub plan_sha256: String,
    pub summaries: Vec<ScopeManifestSummary>,
    pub suite_root: String,
}

struct PersonRendererAuthority {
    plan_digest: String,
    rendered: BTreeMap<String, RenderedSource>,
    projections_by_scope: BTreeMap<String, Vec<crate::persona_plan::SourceProjection>>,
    inventory_by_scope: BTreeMap<String, Vec<PhysicalRawInput>>,
}
impl PersonRendererAuthority {
    fn build_validated(
        plan: &PersonaPlan,
        plan_digest: &str,
        person_id: PersonaId,
    ) -> Result<Self, PersonaManifestError> {
        let person = plan
            .personas
            .iter()
            .find(|person| person.id == person_id)
            .ok_or_else(|| PersonaManifestError::Invalid("unknown shard persona".into()))?;
        let projections = source_projections(person)?;
        let rendered: BTreeMap<String, RenderedSource> =
            render_person_validated(plan, person_id, 0)
                .map_err(|error| {
                    PersonaManifestError::Invalid(format!("renderer rebuild: {error}"))
                })?
                .into_iter()
                .map(|source| (source.source_id.clone(), source))
                .collect();
        let inventory_by_scope = projections
            .iter()
            .map(|projection| {
                let rendered = rendered.get(&projection.source_id).ok_or_else(|| {
                    PersonaManifestError::Invalid("renderer projection missing".into())
                })?;
                Ok((
                    projection.scope_id.clone(),
                    PhysicalRawInput::from_rendered(rendered),
                ))
            })
            .collect::<Result<Vec<_>, PersonaManifestError>>()?
            .into_iter()
            .fold(BTreeMap::new(), |mut grouped, (scope, input)| {
                grouped.entry(scope).or_insert_with(Vec::new).push(input);
                grouped
            });
        let projections_by_scope = projections.into_iter().fold(
            BTreeMap::new(),
            |mut grouped: BTreeMap<_, Vec<_>>, projection| {
                grouped
                    .entry(projection.scope_id.clone())
                    .or_default()
                    .push(projection);
                grouped
            },
        );
        Ok(Self {
            plan_digest: plan_digest.to_owned(),
            rendered,
            projections_by_scope,
            inventory_by_scope,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "record",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum JsonlRecord {
    Header(ManifestHeader),
    PhysicalRaw(PhysicalRaw),
    LogicalItem(LogicalItem),
    SearchableExpectation(SearchableExpectation),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestHeader {
    schema: String,
    plan_sha256: String,
    person_id: PersonaId,
    scope_id: String,
    state_root: String,
    semantic_root: String,
}

fn canonical_line<T: Serialize>(value: &T) -> Result<Vec<u8>, PersonaManifestError> {
    let value =
        serde_json::to_value(value).map_err(|e| PersonaManifestError::Serialize(e.to_string()))?;
    let mut bytes =
        canonical_json_bytes(&value).map_err(|e| PersonaManifestError::Serialize(e.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..].iter().all(u8::is_ascii_hexdigit)
}

fn state_root(
    physical_raw: &[PhysicalRaw],
    logical_items: &[LogicalItem],
    searchable_expectations: &[SearchableExpectation],
) -> Result<String, PersonaManifestError> {
    root(
        "kio.persona.manifest.state/v2",
        physical_raw,
        logical_items,
        searchable_expectations,
    )
}

#[derive(Serialize)]
struct SemanticRaw<'a> {
    source_id: &'a str,
    variant: &'a FormatVariant,
    gate_role: &'a GateRole,
    disposition: &'a Disposition,
    raw_sha256: &'a str,
    byte_len: u64,
    extension: &'a str,
    media_type: &'a str,
}
#[derive(Serialize)]
struct SemanticLogical<'a> {
    logical_id: &'a str,
    source_id: &'a str,
    parent_logical_id: &'a Option<String>,
    contiguous_index: u32,
    source_ordinal: u32,
    unit_kind: &'a UnitKind,
    unit_key: &'a str,
    variant: &'a FormatVariant,
    planned_chunks: u32,
}
#[derive(Serialize)]
struct SemanticSearch<'a> {
    unit_key: &'a str,
    source_id: &'a str,
    variant: &'a FormatVariant,
    gate_role: &'a GateRole,
    disposition: &'a Disposition,
    raw_sha256: &'a str,
    planned_chunks: u32,
    planned_unit_keys: &'a [String],
}

fn semantic_root(
    physical_raw: &[PhysicalRaw],
    logical_items: &[LogicalItem],
    searchable_expectations: &[SearchableExpectation],
) -> Result<String, PersonaManifestError> {
    let raws: Vec<_> = physical_raw
        .iter()
        .map(|r| SemanticRaw {
            source_id: &r.source_id,
            variant: &r.variant,
            gate_role: &r.gate_role,
            disposition: &r.disposition,
            raw_sha256: &r.raw_sha256,
            byte_len: r.byte_len,
            extension: &r.extension,
            media_type: &r.media_type,
        })
        .collect();
    let logical: Vec<_> = logical_items
        .iter()
        .map(|r| SemanticLogical {
            logical_id: &r.logical_id,
            source_id: &r.source_id,
            parent_logical_id: &r.parent_logical_id,
            contiguous_index: r.contiguous_index,
            source_ordinal: r.source_ordinal,
            unit_kind: &r.unit_kind,
            unit_key: &r.unit_key,
            variant: &r.variant,
            planned_chunks: r.planned_chunks,
        })
        .collect();
    let searchable: Vec<_> = searchable_expectations
        .iter()
        .map(|r| SemanticSearch {
            unit_key: &r.unit_key,
            source_id: &r.source_id,
            variant: &r.variant,
            gate_role: &r.gate_role,
            disposition: &r.disposition,
            raw_sha256: &r.raw_sha256,
            planned_chunks: r.planned_chunks,
            planned_unit_keys: &r.planned_unit_keys,
        })
        .collect();
    root(
        "kio.persona.manifest.semantic/v2",
        &raws,
        &logical,
        &searchable,
    )
}

fn root<A: Serialize + ?Sized, B: Serialize + ?Sized, C: Serialize + ?Sized>(
    domain: &str,
    a: &A,
    b: &B,
    c: &C,
) -> Result<String, PersonaManifestError> {
    #[derive(Serialize)]
    struct Root<'a, A: ?Sized, B: ?Sized, C: ?Sized> {
        domain: &'a str,
        physical_raw: &'a A,
        logical_items: &'a B,
        searchable_expectations: &'a C,
    }
    Ok(hash_bytes(&canonical_line(&Root {
        domain,
        physical_raw: a,
        logical_items: b,
        searchable_expectations: c,
    })?))
}

impl ScopeManifest {
    /// Materialize all twenty scope shards for one person.  The renderer is
    /// expanded once; each input remains a compact scope-local inventory.
    pub fn build_person_shards(
        plan: &PersonaPlan,
        person_id: PersonaId,
    ) -> Result<Vec<Self>, PersonaManifestError> {
        let plan_digest = plan.digest()?;
        Self::build_person_shards_validated(plan, &plan_digest, person_id)
    }

    fn build_person_shards_validated(
        plan: &PersonaPlan,
        plan_digest: &str,
        person_id: PersonaId,
    ) -> Result<Vec<Self>, PersonaManifestError> {
        let authority = PersonRendererAuthority::build_validated(plan, plan_digest, person_id)?;
        let person = plan
            .personas
            .iter()
            .find(|person| person.id == person_id)
            .ok_or_else(|| PersonaManifestError::Invalid("unknown shard persona".into()))?;
        let mut shards = Vec::with_capacity(person.scopes.len());
        for scope in &person.scopes {
            let inventory = authority
                .inventory_by_scope
                .get(&scope.id)
                .cloned()
                .unwrap_or_default();
            let shard = Self::from_inventory_with_authority(
                plan, person_id, &scope.id, inventory, &authority,
            )?;
            shards.push(shard);
        }
        Ok(shards)
    }
    /// Constructs all ledger rows from a compact renderer inventory and a
    /// canonical plan.  A source gets precisely one logical item; the renderer
    /// is free to expose logical members in its bytes, but cannot change this
    /// fixture contract's source-level accounting.
    pub fn from_inventory(
        plan: &PersonaPlan,
        person_id: PersonaId,
        scope_id: &str,
        inventory: Vec<PhysicalRawInput>,
    ) -> Result<Self, PersonaManifestError> {
        let plan_digest = plan.digest()?;
        let authority = PersonRendererAuthority::build_validated(plan, &plan_digest, person_id)?;
        Self::from_inventory_with_authority(plan, person_id, scope_id, inventory, &authority)
    }
    fn from_inventory_with_authority(
        plan: &PersonaPlan,
        person_id: PersonaId,
        scope_id: &str,
        inventory: Vec<PhysicalRawInput>,
        authority: &PersonRendererAuthority,
    ) -> Result<Self, PersonaManifestError> {
        let plan_sha256 = authority.plan_digest.clone();
        let mut expected = BTreeMap::new();
        let person = plan
            .personas
            .iter()
            .find(|person| person.id == person_id)
            .ok_or_else(|| PersonaManifestError::Invalid("unknown shard persona".into()))?;
        let scope = person.scopes.iter().find(|scope| scope.id == scope_id);
        if scope.is_none() {
            return Err(PersonaManifestError::Invalid("unknown shard scope".into()));
        }
        let scope_path = &scope
            .ok_or_else(|| PersonaManifestError::Invalid("unknown shard scope".into()))?
            .path;
        if let Some(projections) = authority.projections_by_scope.get(scope_id) {
            for projection in projections.iter().cloned() {
                expected.insert(
                    projection.source_id.clone(),
                    (
                        person_id,
                        projection.scope_id,
                        projection.variant,
                        projection.gate_role,
                        projection.disposition,
                        projection.planned_chunks,
                    ),
                );
            }
        }
        if expected.len() != inventory.len() {
            return Err(PersonaManifestError::Invalid(
                "inventory source count".into(),
            ));
        }
        let mut members_by_source = BTreeMap::new();
        let mut physical_raw = Vec::with_capacity(inventory.len());
        for item in inventory {
            let Some((person_id, scope_id, variant, gate_role, disposition, chunks)) =
                expected.remove(&item.source_id)
            else {
                return Err(PersonaManifestError::Invalid(format!(
                    "unexpected or duplicate source {}",
                    item.source_id
                )));
            };
            let (extension, media_type) = variant_contract(variant);
            if !valid_sha256(&item.raw_sha256)
                || item.byte_len == 0
                || item.byte_len > MAX_RAW_BYTES
            {
                return Err(PersonaManifestError::Invalid(format!(
                    "invalid physical row {}",
                    item.source_id
                )));
            }
            let member_chunks = item
                .logical_members
                .iter()
                .try_fold(0u32, |sum, member| sum.checked_add(member.planned_chunks))
                .ok_or_else(|| {
                    PersonaManifestError::Invalid("logical member chunk overflow".into())
                })?;
            if member_chunks != chunks {
                return Err(PersonaManifestError::Invalid(format!(
                    "logical member chunk marginal {}",
                    item.source_id
                )));
            }
            members_by_source.insert(item.source_id.clone(), item.logical_members);
            physical_raw.push(PhysicalRaw {
                person_id,
                scope_id,
                source_id: item.source_id.clone(),
                variant,
                gate_role,
                disposition,
                raw_sha256: item.raw_sha256,
                byte_len: item.byte_len,
                extension: extension.into(),
                media_type: media_type.into(),
                path: format!("{scope_path}/{}.{}", item.source_id, extension),
            });
        }
        if !expected.is_empty() {
            return Err(PersonaManifestError::Invalid(
                "missing planned inventory source".into(),
            ));
        }
        physical_raw.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        let mut logical_items = Vec::new();
        for raw in &physical_raw {
            let members = members_by_source.get_mut(&raw.source_id).ok_or_else(|| {
                PersonaManifestError::Invalid("missing logical member inventory".into())
            })?;
            members.sort_by_key(|member| member.ordinal);
            for (local_index, member) in members.iter().enumerate() {
                if member.ordinal
                    != u32::try_from(local_index)
                        .map_err(|_| PersonaManifestError::Invalid("logical member count".into()))?
                    || member.key.is_empty()
                {
                    return Err(PersonaManifestError::Invalid(format!(
                        "logical member identity {}",
                        raw.source_id
                    )));
                }
                let parent_logical_id = member
                    .parent_ordinal
                    .map(|parent| format!("{}:logical:{parent:06}", raw.source_id));
                logical_items.push(LogicalItem {
                    logical_id: format!("{}:logical:{:06}", raw.source_id, member.ordinal),
                    source_id: raw.source_id.clone(),
                    parent_logical_id,
                    contiguous_index: u32::try_from(logical_items.len())
                        .map_err(|_| PersonaManifestError::Invalid("logical count".into()))?,
                    source_ordinal: member.ordinal,
                    unit_kind: member.kind,
                    unit_key: format!("{}:{}", raw.source_id, member.key),
                    person_id: raw.person_id,
                    scope_id: raw.scope_id.clone(),
                    variant: raw.variant,
                    planned_chunks: member.planned_chunks,
                });
            }
        }
        let searchable_expectations: Vec<_> = physical_raw
            .iter()
            .map(|raw| -> Result<_, PersonaManifestError> {
                let mut planned_unit_keys: Vec<_> = logical_items
                    .iter()
                    .filter(|logical| logical.source_id == raw.source_id)
                    .map(|logical| logical.unit_key.clone())
                    .collect();
                planned_unit_keys.sort();
                let planned_chunks = logical_items
                    .iter()
                    .filter(|logical| logical.source_id == raw.source_id)
                    .map(|logical| logical.planned_chunks)
                    .try_fold(0u32, |sum, item| sum.checked_add(item))
                    .ok_or_else(|| {
                        PersonaManifestError::Invalid("logical chunk overflow".into())
                    })?;
                Ok(SearchableExpectation {
                    unit_key: format!("{}:expectation", raw.source_id),
                    source_id: raw.source_id.clone(),
                    person_id: raw.person_id,
                    scope_id: raw.scope_id.clone(),
                    variant: raw.variant,
                    gate_role: raw.gate_role,
                    disposition: raw.disposition,
                    raw_sha256: raw.raw_sha256.clone(),
                    path: raw.path.clone(),
                    planned_chunks,
                    planned_unit_keys,
                })
            })
            .collect::<Result<_, _>>()?;
        let mut manifest = Self {
            schema: SCHEMA.into(),
            plan_sha256,
            person_id,
            scope_id: scope_id.into(),
            physical_raw,
            logical_items,
            searchable_expectations,
            state_root: String::new(),
            semantic_root: String::new(),
        };
        manifest.state_root = state_root(
            &manifest.physical_raw,
            &manifest.logical_items,
            &manifest.searchable_expectations,
        )?;
        manifest.semantic_root = semantic_root(
            &manifest.physical_raw,
            &manifest.logical_items,
            &manifest.searchable_expectations,
        )?;
        manifest.validate_with_authority(plan, authority)?;
        Ok(manifest)
    }

    pub fn validate(&self, plan: &PersonaPlan) -> Result<(), PersonaManifestError> {
        let plan_digest = plan.digest()?;
        let authority =
            PersonRendererAuthority::build_validated(plan, &plan_digest, self.person_id)?;
        self.validate_with_authority(plan, &authority)
    }

    fn validate_with_authority(
        &self,
        plan: &PersonaPlan,
        authority: &PersonRendererAuthority,
    ) -> Result<(), PersonaManifestError> {
        if self.schema != SCHEMA || self.plan_sha256 != authority.plan_digest {
            return Err(PersonaManifestError::Invalid(
                "identity or plan binding".into(),
            ));
        }
        let person = plan
            .personas
            .iter()
            .find(|person| person.id == self.person_id)
            .ok_or_else(|| PersonaManifestError::Invalid("unknown shard persona".into()))?;
        let scope = person
            .scopes
            .iter()
            .find(|scope| scope.id == self.scope_id)
            .ok_or_else(|| PersonaManifestError::Invalid("unknown shard scope".into()))?;
        let mut source_ids = BTreeSet::new();
        let mut raw_hashes = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut raw_by_source = BTreeMap::new();
        if !self
            .physical_raw
            .windows(2)
            .all(|pair| pair[0].source_id < pair[1].source_id)
            || !self.logical_items.windows(2).all(|pair| {
                (&pair[0].source_id, pair[0].source_ordinal)
                    < (&pair[1].source_id, pair[1].source_ordinal)
                    && pair[0].contiguous_index < pair[1].contiguous_index
            })
            || !self
                .searchable_expectations
                .windows(2)
                .all(|pair| pair[0].source_id < pair[1].source_id)
        {
            return Err(PersonaManifestError::Invalid("ledger ordering".into()));
        }
        for row in &self.physical_raw {
            if !source_ids.insert(row.source_id.as_str())
                || !raw_hashes.insert(row.raw_sha256.as_str())
                || !paths.insert(row.path.to_ascii_lowercase())
                || !valid_sha256(&row.raw_sha256)
                || row.byte_len == 0
                || row.byte_len > MAX_RAW_BYTES
                || row.extension != variant_contract(row.variant).0
                || row.media_type != variant_contract(row.variant).1
                || row.path != format!("{}/{}.{}", scope.path, row.source_id, row.extension)
            {
                return Err(PersonaManifestError::Invalid(
                    "physical raw uniqueness or shape".into(),
                ));
            }
            raw_by_source.insert(row.source_id.as_str(), row);
        }
        let mut expected = BTreeMap::new();
        if let Some(projections) = authority.projections_by_scope.get(&self.scope_id) {
            for p in projections.iter().cloned() {
                expected.insert(
                    p.source_id,
                    (
                        self.person_id,
                        p.scope_id,
                        p.variant,
                        p.gate_role,
                        p.disposition,
                        p.planned_chunks,
                    ),
                );
            }
        }
        if expected.len() != self.physical_raw.len() {
            return Err(PersonaManifestError::Invalid(
                "physical inventory cardinality".into(),
            ));
        }
        for (source_id, e) in &expected {
            let raw = raw_by_source
                .get(source_id.as_str())
                .ok_or_else(|| PersonaManifestError::Invalid("missing source".into()))?;
            if raw.person_id != e.0
                || raw.scope_id != e.1
                || raw.variant != e.2
                || raw.gate_role != e.3
                || raw.disposition != e.4
            {
                return Err(PersonaManifestError::Invalid(
                    "physical source marginal".into(),
                ));
            }
        }
        let rendered_by_source = &authority.rendered;
        for raw in &self.physical_raw {
            let rendered = rendered_by_source
                .get(&raw.source_id)
                .ok_or_else(|| PersonaManifestError::Invalid("renderer source missing".into()))?;
            if raw.raw_sha256 != rendered.renderer_byte_digest
                || raw.byte_len
                    != u64::try_from(rendered.bytes.len())
                        .map_err(|_| PersonaManifestError::Invalid("renderer length".into()))?
                || raw.extension != rendered.extension
                || raw.media_type != rendered.media_type
            {
                return Err(PersonaManifestError::Invalid(
                    "renderer physical rebuild".into(),
                ));
            }
            let actual: Vec<_> = self
                .logical_items
                .iter()
                .filter(|item| item.source_id == raw.source_id)
                .collect();
            if actual.len() != rendered.logical_members.len() {
                return Err(PersonaManifestError::Invalid(
                    "renderer logical member count".into(),
                ));
            }
            for (item, member) in actual.iter().zip(&rendered.logical_members) {
                if item.unit_kind != member.kind
                    || item.source_ordinal != member.ordinal
                    || item.unit_key != format!("{}:{}", raw.source_id, member.key)
                    || item.planned_chunks != member.planned_chunks
                    || item.parent_logical_id.is_some()
                {
                    return Err(PersonaManifestError::Invalid(
                        "renderer logical rebuild".into(),
                    ));
                }
            }
        }
        let mut logical_ids = BTreeSet::new();
        let mut indices = BTreeSet::new();
        let mut logical_by_id = BTreeMap::new();
        let mut logical_by_source: BTreeMap<&str, Vec<&LogicalItem>> = BTreeMap::new();
        for (position, row) in self.logical_items.iter().enumerate() {
            if !logical_ids.insert(row.logical_id.as_str())
                || !indices.insert(row.contiguous_index)
                || !raw_by_source.contains_key(row.source_id.as_str())
                || usize::try_from(row.contiguous_index) != Ok(position)
            {
                return Err(PersonaManifestError::Invalid(
                    "logical uniqueness or source".into(),
                ));
            }
            let raw = raw_by_source
                .get(row.source_id.as_str())
                .expect("checked source");
            if row.person_id != raw.person_id
                || row.scope_id != raw.scope_id
                || row.logical_id != format!("{}:logical:{:06}", row.source_id, row.source_ordinal)
            {
                return Err(PersonaManifestError::Invalid(
                    "logical identity marginal".into(),
                ));
            }
            logical_by_id.insert(row.logical_id.as_str(), row);
            logical_by_source
                .entry(row.source_id.as_str())
                .or_default()
                .push(row);
        }
        if !indices
            .iter()
            .copied()
            .eq(0..u32::try_from(self.logical_items.len())
                .map_err(|_| PersonaManifestError::Invalid("logical count".into()))?)
        {
            return Err(PersonaManifestError::Invalid(
                "logical cardinality or contiguous indices".into(),
            ));
        }
        for row in &self.logical_items {
            if let Some(parent) = &row.parent_logical_id {
                let parent = logical_by_id.get(parent.as_str()).ok_or_else(|| {
                    PersonaManifestError::Invalid("logical parent missing".into())
                })?;
                if parent.contiguous_index >= row.contiguous_index
                    || parent.source_id != row.source_id
                {
                    return Err(PersonaManifestError::Invalid(
                        "logical parent cycle or source".into(),
                    ));
                }
            }
            let (_, _, variant, _, _, _) = expected
                .get(&row.source_id)
                .ok_or_else(|| PersonaManifestError::Invalid("logical unplanned source".into()))?;
            if &row.variant != variant {
                return Err(PersonaManifestError::Invalid(
                    "logical marginal or chunk sum".into(),
                ));
            }
        }
        for (source_id, (_, _, _, _, _, chunks)) in &expected {
            let members = logical_by_source
                .get(source_id.as_str())
                .map_or(&[][..], Vec::as_slice);
            let mut source_ordinals: Vec<_> =
                members.iter().map(|item| item.source_ordinal).collect();
            source_ordinals.sort_unstable();
            if !source_ordinals
                .iter()
                .copied()
                .eq(0..u32::try_from(members.len())
                    .map_err(|_| PersonaManifestError::Invalid("logical member count".into()))?)
                || members
                    .iter()
                    .try_fold(0u32, |sum, item| sum.checked_add(item.planned_chunks))
                    .ok_or_else(|| PersonaManifestError::Invalid("logical chunk overflow".into()))?
                    != *chunks
            {
                return Err(PersonaManifestError::Invalid(
                    "logical source ordinal or chunk sum".into(),
                ));
            }
        }
        let mut unit_keys = BTreeSet::new();
        let mut expectation_sources = BTreeSet::new();
        for row in &self.searchable_expectations {
            let raw = raw_by_source
                .get(row.source_id.as_str())
                .ok_or_else(|| PersonaManifestError::Invalid("searchable raw missing".into()))?;
            if !unit_keys.insert(row.unit_key.as_str())
                || !expectation_sources.insert(row.source_id.as_str())
                || row.source_id != raw.source_id
                || row.raw_sha256 != raw.raw_sha256
                || row.path != raw.path
                || row.person_id != raw.person_id
                || row.scope_id != raw.scope_id
                || row.variant != raw.variant
                || row.gate_role != raw.gate_role
                || row.disposition != raw.disposition
            {
                return Err(PersonaManifestError::Invalid(
                    "searchable projection".into(),
                ));
            }
        }
        for raw in &self.physical_raw {
            let mut keys: Vec<_> = logical_by_source
                .get(raw.source_id.as_str())
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(|item| item.unit_key.clone())
                .collect();
            keys.sort();
            let chunks: u32 = logical_by_source
                .get(raw.source_id.as_str())
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(|item| item.planned_chunks)
                .try_fold(0u32, |sum, item| sum.checked_add(item))
                .ok_or_else(|| PersonaManifestError::Invalid("logical chunk overflow".into()))?;
            let expectation = self
                .searchable_expectations
                .iter()
                .find(|row| row.source_id == raw.source_id)
                .ok_or_else(|| {
                    PersonaManifestError::Invalid("missing source expectation".into())
                })?;
            if expectation.planned_chunks != chunks
                || expectation.planned_unit_keys != keys
                || !expectation
                    .planned_unit_keys
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            {
                return Err(PersonaManifestError::Invalid(
                    "searchable planned unit projection".into(),
                ));
            }
        }
        if self.searchable_expectations.len() != self.physical_raw.len() {
            return Err(PersonaManifestError::Invalid(
                "searchable cardinality".into(),
            ));
        }
        if self.state_root
            != state_root(
                &self.physical_raw,
                &self.logical_items,
                &self.searchable_expectations,
            )?
            || self.semantic_root
                != semantic_root(
                    &self.physical_raw,
                    &self.logical_items,
                    &self.searchable_expectations,
                )?
        {
            return Err(PersonaManifestError::Invalid("ledger root".into()));
        }
        Ok(())
    }

    pub fn canonical_jsonl(&self) -> Result<Vec<u8>, PersonaManifestError> {
        let mut output = canonical_line(&JsonlRecord::Header(ManifestHeader {
            schema: self.schema.clone(),
            plan_sha256: self.plan_sha256.clone(),
            person_id: self.person_id,
            scope_id: self.scope_id.clone(),
            state_root: self.state_root.clone(),
            semantic_root: self.semantic_root.clone(),
        }))?;
        for row in &self.physical_raw {
            output.extend(canonical_line(&JsonlRecord::PhysicalRaw(row.clone()))?);
        }
        for row in &self.logical_items {
            output.extend(canonical_line(&JsonlRecord::LogicalItem(row.clone()))?);
        }
        for row in &self.searchable_expectations {
            output.extend(canonical_line(&JsonlRecord::SearchableExpectation(
                row.clone(),
            ))?);
        }
        Ok(output)
    }

    pub fn digest(&self) -> Result<String, PersonaManifestError> {
        Ok(hash_bytes(&self.canonical_jsonl()?))
    }

    pub fn summary(&self) -> Result<ScopeManifestSummary, PersonaManifestError> {
        let bytes = self.canonical_jsonl()?;
        let identities = self
            .physical_raw
            .iter()
            .map(|raw| -> Result<_, PersonaManifestError> {
                Ok(IdentityProjection {
                    source_id: raw.source_id.clone(),
                    raw_sha256: raw.raw_sha256.clone(),
                    path_casefolded: raw.path.to_ascii_lowercase(),
                    logical_count: u32::try_from(
                        self.logical_items
                            .iter()
                            .filter(|item| item.source_id == raw.source_id)
                            .count(),
                    )
                    .map_err(|_| PersonaManifestError::Invalid("logical count".into()))?,
                    logical_root: hash_bytes(&canonical_line(
                        &self
                            .logical_items
                            .iter()
                            .filter(|item| item.source_id == raw.source_id)
                            .map(|item| &item.unit_key)
                            .collect::<Vec<_>>(),
                    )?),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ScopeManifestSummary {
            person_id: self.person_id,
            scope_id: self.scope_id.clone(),
            shard_sha256: hash_bytes(&bytes),
            byte_len: u64::try_from(bytes.len())
                .map_err(|_| PersonaManifestError::Invalid("manifest length".into()))?,
            physical_rows: u32::try_from(self.physical_raw.len())
                .map_err(|_| PersonaManifestError::Invalid("physical rows".into()))?,
            logical_rows: u32::try_from(self.logical_items.len())
                .map_err(|_| PersonaManifestError::Invalid("logical rows".into()))?,
            state_root: self.state_root.clone(),
            semantic_root: self.semantic_root.clone(),
            identities,
        })
    }

    pub fn parse_canonical_jsonl(
        bytes: &[u8],
        plan: &PersonaPlan,
    ) -> Result<Self, PersonaManifestError> {
        if bytes.len() > MAX_SCOPE_MANIFEST_BYTES {
            return Err(PersonaManifestError::Invalid("manifest byte limit".into()));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|e| PersonaManifestError::Json(e.to_string()))?;
        if text.is_empty() || !text.ends_with('\n') {
            return Err(PersonaManifestError::NonCanonical);
        }
        let mut records = text.lines().enumerate().map(|(index, line)| {
            if index >= MAX_SCOPE_MANIFEST_ROWS || line.len() > MAX_SCOPE_MANIFEST_LINE_BYTES {
                return Err(PersonaManifestError::Invalid(
                    "manifest row or line limit".into(),
                ));
            }
            serde_json::from_str::<JsonlRecord>(line)
                .map_err(|e| PersonaManifestError::Json(e.to_string()))
        });
        let Some(JsonlRecord::Header(header)) = records.next().transpose()? else {
            return Err(PersonaManifestError::Invalid("missing header".into()));
        };
        let mut physical_raw = Vec::new();
        let mut logical_items = Vec::new();
        let mut searchable_expectations = Vec::new();
        let mut phase = 0;
        for record in records {
            match record? {
                JsonlRecord::Header(_) => {
                    return Err(PersonaManifestError::Invalid("duplicate header".into()));
                }
                JsonlRecord::PhysicalRaw(row) if phase == 0 => physical_raw.push(row),
                JsonlRecord::LogicalItem(row) if phase <= 1 => {
                    phase = 1;
                    logical_items.push(row);
                }
                JsonlRecord::SearchableExpectation(row) => {
                    phase = 2;
                    searchable_expectations.push(row);
                }
                _ => return Err(PersonaManifestError::Invalid("ledger ordering".into())),
            }
        }
        let manifest = Self {
            schema: header.schema,
            plan_sha256: header.plan_sha256,
            person_id: header.person_id,
            scope_id: header.scope_id,
            physical_raw,
            logical_items,
            searchable_expectations,
            state_root: header.state_root,
            semantic_root: header.semantic_root,
        };
        manifest.validate(plan)?;
        if manifest.canonical_jsonl()? != bytes {
            return Err(PersonaManifestError::NonCanonical);
        }
        Ok(manifest)
    }
}

impl SuiteManifest {
    pub fn build(plan: &PersonaPlan) -> Result<Self, PersonaManifestError> {
        let plan_digest = plan.digest()?;
        let mut summaries = Vec::new();
        for person_id in PersonaId::ALL {
            for shard in
                ScopeManifest::build_person_shards_validated(plan, &plan_digest, person_id)?
            {
                summaries.push(shard.summary()?);
            }
        }
        Self::from_summaries_validated(plan, &plan_digest, summaries)
    }
    /// Builds a suite only from validated physical scope shards; callers cannot
    /// fabricate summary roots as fixture authority.
    pub fn from_shards(
        plan: &PersonaPlan,
        shards: impl IntoIterator<Item = ScopeManifest>,
    ) -> Result<Self, PersonaManifestError> {
        let plan_digest = plan.digest()?;
        let authorities: BTreeMap<_, _> = PersonaId::ALL
            .into_iter()
            .map(|person_id| {
                Ok((
                    person_id,
                    PersonRendererAuthority::build_validated(plan, &plan_digest, person_id)?,
                ))
            })
            .collect::<Result<_, PersonaManifestError>>()?;
        let mut summaries = Vec::new();
        for shard in shards {
            let authority = authorities
                .get(&shard.person_id)
                .ok_or_else(|| PersonaManifestError::Invalid("unknown shard persona".into()))?;
            shard.validate_with_authority(plan, authority)?;
            summaries.push(shard.summary()?);
        }
        Self::from_summaries_validated(plan, &plan_digest, summaries)
    }
    fn from_summaries_validated(
        plan: &PersonaPlan,
        plan_digest: &str,
        mut summaries: Vec<ScopeManifestSummary>,
    ) -> Result<Self, PersonaManifestError> {
        if summaries.len() != plan.personas.len() * 20 {
            return Err(PersonaManifestError::Invalid(
                "suite shard cardinality".into(),
            ));
        }
        summaries.sort_by(|a, b| (a.person_id, &a.scope_id).cmp(&(b.person_id, &b.scope_id)));
        let mut expected_by_scope: BTreeMap<(PersonaId, String), BTreeSet<String>> =
            BTreeMap::new();
        let mut expected_sources = BTreeSet::new();
        for person in &plan.personas {
            for source in source_projections(person)? {
                expected_sources.insert(source.source_id.clone());
                expected_by_scope
                    .entry((person.id, source.scope_id))
                    .or_default()
                    .insert(source.source_id);
            }
        }
        let mut shards = BTreeSet::new();
        let mut sources = BTreeSet::new();
        let mut hashes = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for summary in &summaries {
            if !shards.insert((summary.person_id, summary.scope_id.as_str()))
                || !valid_sha256(&summary.shard_sha256)
                || summary.byte_len == 0
                || (usize::try_from(summary.physical_rows) != Ok(summary.identities.len()))
                || !valid_sha256(&summary.state_root)
                || !valid_sha256(&summary.semantic_root)
            {
                return Err(PersonaManifestError::Invalid("suite summary shape".into()));
            }
            for identity in &summary.identities {
                if !sources.insert(identity.source_id.as_str())
                    || !hashes.insert(identity.raw_sha256.as_str())
                    || !paths.insert(identity.path_casefolded.as_str())
                    || !valid_sha256(&identity.raw_sha256)
                {
                    return Err(PersonaManifestError::Invalid(
                        "suite duplicate inventory identity".into(),
                    ));
                }
                if identity.logical_count == 0 || !valid_sha256(&identity.logical_root) {
                    return Err(PersonaManifestError::Invalid(
                        "suite logical projection".into(),
                    ));
                }
            }
            let actual: BTreeSet<_> = summary
                .identities
                .iter()
                .map(|identity| identity.source_id.as_str())
                .collect();
            let expected_scope = expected_by_scope
                .get(&(summary.person_id, summary.scope_id.clone()))
                .ok_or_else(|| PersonaManifestError::Invalid("suite summary scope".into()))?;
            if actual != expected_scope.iter().map(String::as_str).collect()
                || summary.logical_rows
                    != summary
                        .identities
                        .iter()
                        .try_fold(0u32, |sum, identity| {
                            sum.checked_add(identity.logical_count)
                        })
                        .ok_or_else(|| {
                            PersonaManifestError::Invalid("suite logical row overflow".into())
                        })?
            {
                return Err(PersonaManifestError::Invalid(
                    "suite shard source coverage or logical rows".into(),
                ));
            }
        }
        let expected: BTreeSet<_> = plan
            .personas
            .iter()
            .flat_map(|person| {
                person
                    .scopes
                    .iter()
                    .map(move |scope| (person.id, scope.id.as_str()))
            })
            .collect();
        if shards != expected {
            return Err(PersonaManifestError::Invalid(
                "suite shard plan coverage".into(),
            ));
        }
        let actual_sources: BTreeSet<_> = summaries
            .iter()
            .flat_map(|summary| {
                summary
                    .identities
                    .iter()
                    .map(|identity| identity.source_id.clone())
            })
            .collect();
        if actual_sources != expected_sources {
            return Err(PersonaManifestError::Invalid(
                "suite source plan coverage".into(),
            ));
        }
        let plan_sha256 = plan_digest.to_owned();
        let suite_root = suite_root(&plan_sha256, &summaries)?;
        Ok(Self {
            schema: SCHEMA.into(),
            plan_sha256,
            summaries,
            suite_root,
        })
    }
    pub fn validate(&self, plan: &PersonaPlan) -> Result<(), PersonaManifestError> {
        let plan_digest = plan.digest()?;
        if self.schema != SCHEMA || self.plan_sha256 != plan_digest {
            return Err(PersonaManifestError::Invalid(
                "suite identity or plan binding".into(),
            ));
        }
        let mut summaries = Vec::new();
        for person_id in PersonaId::ALL {
            for shard in
                ScopeManifest::build_person_shards_validated(plan, &plan_digest, person_id)?
            {
                summaries.push(shard.summary()?);
            }
        }
        let rebuilt = Self::from_summaries_validated(plan, &plan_digest, summaries)?;
        if rebuilt != *self {
            return Err(PersonaManifestError::Invalid(
                "suite canonical rebuild".into(),
            ));
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PersonaManifestError> {
        canonical_line(self)
    }
    pub fn digest(&self) -> Result<String, PersonaManifestError> {
        Ok(hash_bytes(&self.canonical_bytes()?))
    }
    pub fn parse_canonical(bytes: &[u8], plan: &PersonaPlan) -> Result<Self, PersonaManifestError> {
        if bytes.len() > MAX_SUITE_MANIFEST_BYTES {
            return Err(PersonaManifestError::Invalid("suite byte limit".into()));
        }
        let suite: Self =
            serde_json::from_slice(bytes).map_err(|e| PersonaManifestError::Json(e.to_string()))?;
        if suite.summaries.len() > MAX_SUITE_SUMMARIES
            || suite
                .summaries
                .iter()
                .any(|summary| summary.identities.len() > MAX_SUMMARY_IDENTITIES)
        {
            return Err(PersonaManifestError::Invalid(
                "suite summary or identity limit".into(),
            ));
        }
        suite.validate(plan)?;
        if suite.canonical_bytes()? != bytes {
            return Err(PersonaManifestError::NonCanonical);
        }
        Ok(suite)
    }
}

fn suite_root(
    plan_sha256: &str,
    summaries: &[ScopeManifestSummary],
) -> Result<String, PersonaManifestError> {
    #[derive(Serialize)]
    struct Root<'a> {
        domain: &'a str,
        plan_sha256: &'a str,
        summaries: &'a [ScopeManifestSummary],
    }
    Ok(hash_bytes(&canonical_line(&Root {
        domain: "kio.persona.manifest.suite/v2",
        plan_sha256,
        summaries,
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona_plan::{PersonaProfile, frozen_plan};
    use crate::persona_render::render_person;

    fn inventory(
        plan: &PersonaPlan,
        person_id: PersonaId,
        scope_id: &str,
    ) -> Vec<PhysicalRawInput> {
        let person = plan
            .personas
            .iter()
            .find(|person| person.id == person_id)
            .unwrap();
        let source_ids: BTreeSet<_> = source_projections(person)
            .unwrap()
            .into_iter()
            .filter(|source| source.scope_id == scope_id)
            .map(|source| source.source_id)
            .collect();
        render_person(plan, person_id, 0)
            .unwrap()
            .into_iter()
            .filter(|source| source_ids.contains(&source.source_id))
            .map(|source| PhysicalRawInput::from_rendered(&source))
            .collect()
    }

    #[test]
    fn inventory_is_compact_deterministic_and_plan_bound() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let person_id = PersonaId::P01;
        let scope_id = "p01-primary-01";
        let a = ScopeManifest::from_inventory(
            &plan,
            person_id,
            scope_id,
            inventory(&plan, person_id, scope_id),
        )
        .unwrap();
        a.validate(&plan).unwrap();
        let bytes = a.canonical_jsonl().unwrap();
        assert_eq!(
            ScopeManifest::parse_canonical_jsonl(&bytes, &plan).unwrap(),
            a
        );
        assert_eq!(
            a.digest().unwrap(),
            ScopeManifest::from_inventory(
                &plan,
                person_id,
                scope_id,
                inventory(&plan, person_id, scope_id)
            )
            .unwrap()
            .digest()
            .unwrap()
        );
        assert_eq!(
            a.digest().unwrap(),
            "sha256:5aa40dc97d9f30dad0219d3d1e2882719d4dfc515fe2cb410ceb6e459baec57f"
        );
        assert!(a.physical_raw.len() < 200);
        assert!(a.canonical_jsonl().unwrap().len() < plan.canonical_bytes().unwrap().len());
        assert!(
            a.searchable_expectations
                .iter()
                .any(|row| row.gate_role == GateRole::RawOnly && row.planned_chunks == 0)
        );
    }

    #[test]
    fn rejects_malformed_legacy_unknown_duplicate_and_noncanonical_jsonl() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let manifest = ScopeManifest::from_inventory(
            &plan,
            PersonaId::P01,
            "p01-primary-01",
            inventory(&plan, PersonaId::P01, "p01-primary-01"),
        )
        .unwrap();
        let bytes = manifest.canonical_jsonl().unwrap();
        let mut unknown = String::from_utf8(bytes.clone()).unwrap();
        unknown = unknown.replacen(
            "\"record\":\"header\"",
            "\"extra\":true,\"record\":\"header\"",
            1,
        );
        assert!(ScopeManifest::parse_canonical_jsonl(unknown.as_bytes(), &plan).is_err());
        let duplicate = [
            bytes.clone(),
            canonical_line(&JsonlRecord::PhysicalRaw(manifest.physical_raw[0].clone())).unwrap(),
        ]
        .concat();
        assert!(ScopeManifest::parse_canonical_jsonl(&duplicate, &plan).is_err());
        assert!(ScopeManifest::parse_canonical_jsonl(&bytes[..bytes.len() - 1], &plan).is_err());
        assert!(ScopeManifest::parse_canonical_jsonl(b"{\"schema\":1}\n", &plan).is_err());
    }

    #[test]
    fn placement_changes_state_but_not_semantic_root() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let a = ScopeManifest::from_inventory(
            &plan,
            PersonaId::P01,
            "p01-primary-01",
            inventory(&plan, PersonaId::P01, "p01-primary-01"),
        )
        .unwrap();
        let mut b = a.clone();
        b.physical_raw[0].path = "p01/relocated.fixture".into();
        let source = b.physical_raw[0].source_id.clone();
        b.searchable_expectations
            .iter_mut()
            .find(|e| e.source_id == source)
            .unwrap()
            .path = "p01/relocated.fixture".into();
        b.state_root = state_root(
            &b.physical_raw,
            &b.logical_items,
            &b.searchable_expectations,
        )
        .unwrap();
        b.semantic_root = semantic_root(
            &b.physical_raw,
            &b.logical_items,
            &b.searchable_expectations,
        )
        .unwrap();
        assert_ne!(a.state_root, b.state_root);
        assert_eq!(a.semantic_root, b.semantic_root);
    }

    #[test]
    fn preserves_multi_member_projection_and_rejects_parent_cycles() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let mut rows = inventory(&plan, PersonaId::P01, "p01-primary-01");
        let row = rows
            .iter_mut()
            .find(|row| !row.logical_members.is_empty())
            .unwrap();
        let chunks = row.logical_members[0].planned_chunks;
        row.logical_members = vec![
            LogicalMemberInput {
                kind: UnitKind::Page,
                key: "page:1".into(),
                ordinal: 0,
                planned_chunks: chunks - 1,
                parent_ordinal: None,
            },
            LogicalMemberInput {
                kind: UnitKind::Image,
                key: "image:1".into(),
                ordinal: 1,
                planned_chunks: 1,
                parent_ordinal: Some(0),
            },
        ];
        assert!(
            ScopeManifest::from_inventory(&plan, PersonaId::P01, "p01-primary-01", rows).is_err()
        );
    }

    #[test]
    fn scope_rejects_reordered_and_forged_logical_rows() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let manifest = ScopeManifest::from_inventory(
            &plan,
            PersonaId::P01,
            "p01-primary-01",
            inventory(&plan, PersonaId::P01, "p01-primary-01"),
        )
        .unwrap();

        let mut reordered = manifest.clone();
        reordered.physical_raw.swap(0, 1);
        assert!(reordered.validate(&plan).is_err());

        let mut reordered = manifest.clone();
        reordered.searchable_expectations.swap(0, 1);
        assert!(reordered.validate(&plan).is_err());

        let mut forged = manifest.clone();
        forged.logical_items[0].person_id = PersonaId::P02;
        assert!(forged.validate(&plan).is_err());
        let mut forged = manifest.clone();
        forged.logical_items[0].scope_id = "p01-primary-02".into();
        assert!(forged.validate(&plan).is_err());
        let mut forged = manifest;
        forged.logical_items[0].logical_id = "forged:logical:000000".into();
        assert!(forged.validate(&plan).is_err());
    }

    #[test]
    fn suite_parser_enforces_pre_deserialization_bounds() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        assert!(
            SuiteManifest::parse_canonical(&vec![b' '; MAX_SUITE_MANIFEST_BYTES + 1], &plan)
                .is_err()
        );
        let summary = ScopeManifestSummary {
            person_id: PersonaId::P01,
            scope_id: "p01-primary-01".into(),
            shard_sha256: hash_bytes(b"shard"),
            byte_len: 1,
            physical_rows: 0,
            logical_rows: 0,
            state_root: hash_bytes(b"state"),
            semantic_root: hash_bytes(b"semantic"),
            identities: Vec::new(),
        };
        let oversized = SuiteManifest {
            schema: SCHEMA.into(),
            plan_sha256: plan.digest().unwrap(),
            summaries: vec![summary; MAX_SUITE_SUMMARIES + 1],
            suite_root: hash_bytes(b"suite"),
        };
        assert!(
            SuiteManifest::parse_canonical(&oversized.canonical_bytes().unwrap(), &plan).is_err()
        );
    }

    #[test]
    fn tiny_all_person_shards_are_renderer_bound_and_frozen() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let suite = SuiteManifest::build(&plan).unwrap();
        assert_eq!(suite.summaries.len(), 400);
        SuiteManifest::parse_canonical(&suite.canonical_bytes().unwrap(), &plan).unwrap();
        assert_eq!(
            suite.digest().unwrap(),
            "sha256:f4b2fcddcb9092a721d1d8119438b2ab4a1ded6946c9ed91b0d097bd211deaa7"
        );

        let mut fabricated = suite;
        fabricated.summaries[0].identities[0].raw_sha256 = hash_bytes(b"fabricated raw");
        fabricated.suite_root = suite_root(&fabricated.plan_sha256, &fabricated.summaries).unwrap();
        assert!(fabricated.validate(&plan).is_err());
    }
}
