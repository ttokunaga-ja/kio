//! Compact, filesystem-independent receipt for plan-owned persona rendering.
//!
//! This is intentionally not a corpus manifest or Kio attestation.  It records
//! only deterministic renderer facts which can be rebuilt from the Rust plan.

use crate::{
    persona_plan::{PersonaId, PersonaPlan, PersonaProfile, TransformKind, source_projections},
    persona_render::{
        FROZEN_RENDER_VECTOR, LogicalMember, RENDERER_ID, RENDERER_SCHEMA, RenderedSource,
        TransformWitness, UnitKind, render_person_validated, render_structural_validated,
    },
};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Deserializer, Serialize, de::Visitor};
use std::collections::BTreeSet;
use thiserror::Error;

pub const SCHEMA: &str = "kio.persona.render-artifact/v2";
pub const TINY_ARTIFACT_HASH: &str =
    "sha256:6e44303e351ccb8c92c29a675050742d8b8e265628a7923f8aeac2327f2ea5db";
pub const PILOT_ARTIFACT_HASH: &str =
    "sha256:d2910a32e1204e7cd758bb686d30b814dde4b3c2607d89c068b51a4c43a0d9ea";
pub const FULL_ARTIFACT_HASH: &str =
    "sha256:5d529e8a00deea2e37162fb8c479e18baee23fa22bbadf4154cf76a3c396d856";
pub const MAX_SOURCES: usize = 200_000;
pub const MAX_CANONICAL_BYTES: usize = 160 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 128;
const MAX_JSON_STRING_BYTES: usize = 8 * 1024;
const MAX_OBJECT_MEMBERS: usize = 16;
const MAX_LOGICAL_MEMBERS_PER_SOURCE: usize = 128;
const MAX_TRANSFORMS_PER_PERSON: usize = 64;
const MAX_CONTAINER_ELEMENTS: usize = 16_000;
// The frozen Full artifact contains 17,327,512 structural JSON tokens.  Keep
// this pre-Serde scan bounded while admitting the largest canonical profile.
// Any renderer-schema change must still update the frozen artifact vectors.
const MAX_TOKENS: usize = MAX_SOURCES * 90;
const MAX_OBJECTS: usize = MAX_SOURCES * 20;
const MAX_ARRAYS: usize = MAX_SOURCES * 4;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RenderArtifactError {
    #[error("plan: {0}")]
    Plan(String),
    #[error("render: {0}")]
    Render(String),
    #[error("serialization: {0}")]
    Serialize(String),
    #[error("JSON: {0}")]
    Json(String),
    #[error("noncanonical JCS+LF")]
    NonCanonical,
    #[error("invalid render artifact: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderArtifact {
    pub schema: String,
    pub fixture_id: String,
    pub profile: PersonaProfile,
    pub plan_digest: String,
    pub renderer_id: String,
    pub renderer_schema: String,
    pub frozen_render_vector: String,
    #[serde(deserialize_with = "bounded_people")]
    pub people: Vec<RenderPerson>,
    pub source_count: u32,
    pub artifact_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderPerson {
    pub persona_id: PersonaId,
    #[serde(deserialize_with = "bounded_sources")]
    pub sources: Vec<RenderSource>,
    #[serde(deserialize_with = "bounded_transforms")]
    pub transforms: Vec<RenderTransform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderSource {
    pub source_id: String,
    pub scope_id: String,
    pub planned_identity: String,
    pub renderer_byte_digest: String,
    pub extension: String,
    pub media_type: String,
    pub planned_chunks: u32,
    #[serde(deserialize_with = "bounded_members")]
    pub logical_members: Vec<RenderMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderMember {
    pub key: String,
    pub kind: UnitKind,
    pub ordinal: u32,
    pub planned_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderTransform {
    pub event_id: String,
    pub source_id: String,
    pub parent_source_id: String,
    pub transform: TransformKind,
    pub witness: RenderWitness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderWitness {
    pub parent_planned_identity: String,
    pub child_planned_identity: String,
    pub changed_channel: Option<u8>,
    pub parent_renderer_byte_digest: String,
    pub child_renderer_byte_digest: String,
    pub parent_pixel_digest: String,
    pub parent_pixel_len: u8,
}

impl RenderArtifact {
    pub fn build(plan: &PersonaPlan) -> Result<Self, RenderArtifactError> {
        plan.validate()
            .map_err(|e| RenderArtifactError::Plan(e.to_string()))?;
        let plan_digest = plan
            .digest()
            .map_err(|e| RenderArtifactError::Plan(e.to_string()))?;
        let mut people = Vec::with_capacity(plan.personas.len());
        let mut total = 0usize;
        for person in &plan.personas {
            // A person's raw render bytes live only for this iteration.
            let rendered = render_person_validated(plan, person.id, 0)
                .map_err(|e| RenderArtifactError::Render(e.to_string()))?;
            total = total
                .checked_add(rendered.len())
                .ok_or_else(|| RenderArtifactError::Invalid("source count overflow".into()))?;
            if total > MAX_SOURCES {
                return Err(RenderArtifactError::Invalid("source capacity".into()));
            }
            let projections =
                source_projections(person).map_err(|e| RenderArtifactError::Plan(e.to_string()))?;
            if projections.len() != rendered.len() {
                return Err(RenderArtifactError::Invalid(
                    "source projection count".into(),
                ));
            }
            let sources = rendered
                .iter()
                .zip(projections.iter())
                .map(|(rendered, projected)| source_record(rendered, &projected.scope_id))
                .collect::<Vec<_>>();
            let mut transforms = Vec::new();
            for event in &person.structural {
                if event.transform == TransformKind::CanonicalSource {
                    continue;
                }
                let parent_id = event
                    .parent_source_id
                    .as_deref()
                    .ok_or_else(|| RenderArtifactError::Invalid("transform parent".into()))?;
                let parent = rendered
                    .iter()
                    .find(|item| item.source_id == parent_id)
                    .ok_or_else(|| {
                        RenderArtifactError::Invalid("transform parent is absent".into())
                    })?;
                let (child, witness) =
                    render_structural_validated(plan, &event.event_id, parent, 0)
                        .map_err(|e| RenderArtifactError::Render(e.to_string()))?;
                transforms.push(RenderTransform {
                    event_id: event.event_id.clone(),
                    source_id: event.source_id.clone(),
                    parent_source_id: parent_id.to_owned(),
                    transform: event.transform,
                    witness: witness_record(&witness),
                });
                drop(child);
            }
            people.push(RenderPerson {
                persona_id: person.id,
                sources,
                transforms,
            });
        }
        let source_count = u32::try_from(total)
            .map_err(|_| RenderArtifactError::Invalid("source count".into()))?;
        let mut artifact = Self {
            schema: SCHEMA.into(),
            fixture_id: plan.fixture_id.clone(),
            profile: plan.profile,
            plan_digest,
            renderer_id: RENDERER_ID.into(),
            renderer_schema: RENDERER_SCHEMA.into(),
            frozen_render_vector: FROZEN_RENDER_VECTOR.into(),
            people,
            source_count,
            artifact_digest: String::new(),
        };
        artifact.artifact_digest = artifact.expected_digest()?;
        artifact.validate_syntax()?;
        Ok(artifact)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RenderArtifactError> {
        self.validate_syntax()?;
        let mut bytes = canonical_json_bytes(
            &serde_json::to_value(self)
                .map_err(|e| RenderArtifactError::Serialize(e.to_string()))?,
        )
        .map_err(|e| RenderArtifactError::Serialize(e.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_CANONICAL_BYTES {
            return Err(RenderArtifactError::Invalid(
                "canonical byte capacity".into(),
            ));
        }
        Ok(bytes)
    }

    pub fn parse_canonical(plan: &PersonaPlan, bytes: &[u8]) -> Result<Self, RenderArtifactError> {
        preflight_json(bytes)?;
        let artifact: Self =
            serde_json::from_slice(bytes).map_err(|e| RenderArtifactError::Json(e.to_string()))?;
        if artifact.canonical_bytes()? != bytes {
            return Err(RenderArtifactError::NonCanonical);
        }
        if artifact != Self::build(plan)? {
            return Err(RenderArtifactError::Invalid(
                "artifact differs from plan rebuild".into(),
            ));
        }
        Ok(artifact)
    }

    fn expected_digest(&self) -> Result<String, RenderArtifactError> {
        let mut value = serde_json::to_value(self)
            .map_err(|e| RenderArtifactError::Serialize(e.to_string()))?;
        value
            .as_object_mut()
            .ok_or_else(|| RenderArtifactError::Invalid("artifact object".into()))?
            .insert(
                "artifact_digest".into(),
                serde_json::Value::String(String::new()),
            );
        Ok(hash_bytes(&canonical_json_bytes(&value).map_err(|e| {
            RenderArtifactError::Serialize(e.to_string())
        })?))
    }

    fn validate_syntax(&self) -> Result<(), RenderArtifactError> {
        if self.schema != SCHEMA
            || self.renderer_id != RENDERER_ID
            || self.renderer_schema != RENDERER_SCHEMA
            || self.frozen_render_vector != FROZEN_RENDER_VECTOR
            || !sha256(&self.plan_digest)
            || self.people.len() != 20
        {
            return Err(RenderArtifactError::Invalid("identity".into()));
        }
        let mut people = BTreeSet::new();
        let mut sources = BTreeSet::new();
        let mut count = 0usize;
        for person in &self.people {
            if !people.insert(person.persona_id) {
                return Err(RenderArtifactError::Invalid("persona identity".into()));
            }
            for source in &person.sources {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("source count overflow".into()))?;
                if source.source_id.is_empty()
                    || source.scope_id.is_empty()
                    || !sha256(&source.planned_identity)
                    || !sha256(&source.renderer_byte_digest)
                    || !sources.insert(&source.source_id)
                {
                    return Err(RenderArtifactError::Invalid("source identity".into()));
                }
                if source.logical_members.len() > MAX_LOGICAL_MEMBERS_PER_SOURCE {
                    return Err(RenderArtifactError::Invalid(
                        "logical member capacity".into(),
                    ));
                }
            }
            if person.transforms.len() > MAX_TRANSFORMS_PER_PERSON {
                return Err(RenderArtifactError::Invalid("transform capacity".into()));
            }
        }
        if count > MAX_SOURCES
            || u32::try_from(count).ok() != Some(self.source_count)
            || self.artifact_digest != self.expected_digest()?
        {
            return Err(RenderArtifactError::Invalid("count or digest".into()));
        }
        Ok(())
    }
}

fn source_record(source: &RenderedSource, scope_id: &str) -> RenderSource {
    RenderSource {
        source_id: source.source_id.clone(),
        scope_id: scope_id.into(),
        planned_identity: source.planned_identity.clone(),
        renderer_byte_digest: source.renderer_byte_digest.clone(),
        extension: source.extension.into(),
        media_type: source.media_type.into(),
        planned_chunks: source.planned_chunks,
        logical_members: source.logical_members.iter().map(member_record).collect(),
    }
}
fn member_record(member: &LogicalMember) -> RenderMember {
    RenderMember {
        key: member.key.clone(),
        kind: member.kind,
        ordinal: member.ordinal,
        planned_chunks: member.planned_chunks,
    }
}
fn witness_record(w: &TransformWitness) -> RenderWitness {
    RenderWitness {
        parent_planned_identity: w.parent_planned_identity.clone(),
        child_planned_identity: w.child_planned_identity.clone(),
        changed_channel: w.changed_channel,
        parent_renderer_byte_digest: w.parent_renderer_byte_digest.clone(),
        child_renderer_byte_digest: w.child_renderer_byte_digest.clone(),
        parent_pixel_digest: w.parent_pixel_digest.clone(),
        parent_pixel_len: w.parent_pixel_len,
    }
}

fn bounded_vec<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for Bounded<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX} entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
            while let Some(value) = seq.next_element()? {
                if out.len() == MAX {
                    return Err(serde::de::Error::custom("array capacity"));
                }
                out.push(value);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, MAX>(std::marker::PhantomData))
}
fn bounded_people<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 20>(d)
}
fn bounded_sources<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, 16_000>(d)
}
fn bounded_transforms<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, MAX_TRANSFORMS_PER_PERSON>(d)
}
fn bounded_members<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    bounded_vec::<D, T, MAX_LOGICAL_MEMBERS_PER_SOURCE>(d)
}

fn sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Bound allocations before feeding untrusted bytes to serde.  This remains a
/// lexical guard, not a replacement JSON grammar parser.
fn preflight_json(bytes: &[u8]) -> Result<(), RenderArtifactError> {
    if bytes.len() > MAX_CANONICAL_BYTES {
        return Err(RenderArtifactError::Invalid(
            "canonical byte capacity".into(),
        ));
    }
    let (mut depth, mut string, mut escaped, mut in_string) = (0usize, 0usize, false, false);
    let mut containers: Vec<(u8, usize)> = Vec::new();
    let (mut tokens, mut objects, mut arrays) = (0usize, 0usize, 0usize);
    for &byte in bytes {
        if in_string {
            string = string
                .checked_add(1)
                .ok_or_else(|| RenderArtifactError::Invalid("string overflow".into()))?;
            if string > MAX_JSON_STRING_BYTES {
                return Err(RenderArtifactError::Invalid("JSON string capacity".into()));
            }
            if escaped {
                escaped = false;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                in_string = false;
                continue;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string = 0;
            }
            b'{' | b'[' => {
                tokens = tokens
                    .checked_add(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("JSON token capacity".into()))?;
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("JSON depth".into()))?;
                if depth > MAX_JSON_DEPTH {
                    return Err(RenderArtifactError::Invalid("JSON depth".into()));
                }
                if byte == b'{' {
                    objects = objects.checked_add(1).ok_or_else(|| {
                        RenderArtifactError::Invalid("JSON object capacity".into())
                    })?;
                    if objects > MAX_OBJECTS {
                        return Err(RenderArtifactError::Invalid("JSON object capacity".into()));
                    }
                } else {
                    arrays = arrays.checked_add(1).ok_or_else(|| {
                        RenderArtifactError::Invalid("JSON array capacity".into())
                    })?;
                    if arrays > MAX_ARRAYS {
                        return Err(RenderArtifactError::Invalid("JSON array capacity".into()));
                    }
                }
                containers.push((byte, 0));
            }
            b'}' | b']' => {
                tokens = tokens
                    .checked_add(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("JSON token capacity".into()))?;
                containers.pop();
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("JSON structure".into()))?;
            }
            b',' => {
                tokens = tokens
                    .checked_add(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("JSON token capacity".into()))?;
                if let Some((kind, elements)) = containers.last_mut() {
                    *elements = elements.checked_add(1).ok_or_else(|| {
                        RenderArtifactError::Invalid("JSON member capacity".into())
                    })?;
                    let maximum = if *kind == b'{' {
                        MAX_OBJECT_MEMBERS
                    } else {
                        MAX_CONTAINER_ELEMENTS
                    };
                    if *elements >= maximum {
                        return Err(RenderArtifactError::Invalid(
                            "JSON container element capacity".into(),
                        ));
                    }
                }
            }
            b':' => {
                tokens = tokens
                    .checked_add(1)
                    .ok_or_else(|| RenderArtifactError::Invalid("JSON token capacity".into()))?;
            }
            _ => {}
        }
        if tokens > MAX_TOKENS {
            return Err(RenderArtifactError::Invalid("JSON token capacity".into()));
        }
    }
    if in_string || depth != 0 {
        return Err(RenderArtifactError::Invalid("JSON structure".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona_plan::{PersonaProfile, frozen_plan};
    #[test]
    fn tiny_artifact_is_canonical_and_has_no_payloads() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let artifact = RenderArtifact::build(&plan).unwrap();
        let bytes = artifact.canonical_bytes().unwrap();
        assert_eq!(artifact.artifact_digest, TINY_ARTIFACT_HASH);
        assert_eq!(
            RenderArtifact::parse_canonical(&plan, &bytes).unwrap(),
            artifact
        );
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("history_ready"));
        assert!(!text.contains("\"bytes\""));
        assert!(!text.contains("kio_observed"));
    }
    #[test]
    fn tiny_and_pilot_artifacts_fit_the_explicit_bounds() {
        for (profile, expected_sources, expected_digest) in [
            (PersonaProfile::Tiny, 4_000, TINY_ARTIFACT_HASH),
            (PersonaProfile::Pilot, 20_000, PILOT_ARTIFACT_HASH),
        ] {
            let plan = frozen_plan(profile);
            let artifact = RenderArtifact::build(&plan).unwrap();
            assert_eq!(artifact.source_count, expected_sources);
            assert_eq!(artifact.artifact_digest, expected_digest);
            assert!(artifact.source_count as usize <= MAX_SOURCES);
            let bytes = artifact.canonical_bytes().unwrap();
            assert!(bytes.len() <= MAX_CANONICAL_BYTES);
            preflight_json(&bytes).unwrap();
        }
    }
    #[test]
    fn preflight_rejects_escaped_string_and_container_floods_before_serde() {
        let escaped = format!("\"{}\"", "\\\\".repeat(MAX_JSON_STRING_BYTES));
        assert!(preflight_json(escaped.as_bytes()).is_err());
        let members = format!("{{{}}}", ",".repeat(MAX_OBJECT_MEMBERS));
        assert!(preflight_json(members.as_bytes()).is_err());
        let elements = format!("[{}]", ",".repeat(MAX_CONTAINER_ELEMENTS));
        assert!(preflight_json(elements.as_bytes()).is_err());
    }
    #[test]
    fn bounded_deserialization_rejects_extra_people() {
        let artifact = RenderArtifact::build(&frozen_plan(PersonaProfile::Tiny)).unwrap();
        let mut value = serde_json::to_value(artifact).unwrap();
        let people = value["people"].as_array_mut().unwrap();
        people.push(people[0].clone());
        assert!(serde_json::from_value::<RenderArtifact>(value).is_err());
    }
}
