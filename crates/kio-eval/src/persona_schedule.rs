//! Deterministic, plan-owned persona history schedules.
//!
//! A schedule describes intended fixture history only.  It neither publishes
//! bytes nor asserts that Kio observed or indexed any of the described items.

use crate::persona_plan::{
    Boundary, Cohort, Operation, PersonPlan, PersonaId, PersonaPlan, StructuralKind, Wave,
    source_projections,
};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const SCHEMA: &str = "kio.persona.schedule/v2";
const MAX_EVENTS_PER_PERSON: usize = 100_000;
pub const MAX_CANONICAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 128;
const MAX_JSON_STRING_BYTES: usize = 8 * 1024;
const MAX_SUITE_PROJECTIONS: usize = MAX_EVENTS_PER_PERSON * 20;
const MAX_OBJECT_MEMBERS: usize = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PersonaScheduleError {
    #[error("plan: {0}")]
    Plan(String),
    #[error("serialization: {0}")]
    Serialize(String),
    #[error("JSON: {0}")]
    Json(String),
    #[error("noncanonical JCS+LF")]
    NonCanonical,
    #[error("invalid persona schedule: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPhase {
    Regular,
    IndexAuto,
    Purge,
    IndexNoop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOrigin {
    History,
    Structural,
    Boundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAction {
    Edit,
    DeleteAndReplace,
    CreateReplacement,
    Purge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEvent {
    pub event_id: String,
    pub item_id: String,
    pub logical_tick: u32,
    pub timestamp: String,
    pub wave: Wave,
    pub phase: EventPhase,
    pub origin: EventOrigin,
    pub history_action: Option<HistoryAction>,
    pub operation: Operation,
    pub boundary: Boundary,
    pub source_id: Option<String>,
    pub scope_id: Option<String>,
    pub cohort: Option<Cohort>,
    pub depends_on: Vec<String>,
    pub prior_item_id: Option<String>,
    pub versions: Vec<u8>,
    pub paired_event_id: Option<String>,
    pub old_planned_identity: Option<String>,
    pub new_planned_identity: Option<String>,
    pub purged_version_identities: Vec<String>,
    pub current_chunks_delta: i32,
    pub history_chunks_delta: i32,
    pub physical_files_delta: i8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonSchedule {
    pub schema: String,
    pub persona_id: PersonaId,
    pub plan_digest: String,
    pub events: Vec<ScheduleEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonScheduleSummary {
    pub persona_id: PersonaId,
    pub event_count: u32,
    pub schedule_digest: String,
    pub projection_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteSchedule {
    pub schema: String,
    pub plan_digest: String,
    pub people: Vec<PersonScheduleSummary>,
    pub projections: Vec<SuiteProjection>,
    pub suite_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteProjection {
    pub suite_ordinal: u32,
    pub persona_id: PersonaId,
    pub local_tick: u32,
    pub event_id: String,
    pub item_id: String,
    pub wave: Wave,
    pub phase: EventPhase,
    pub planned_identity: String,
    pub prior_item_id: Option<String>,
}

impl PersonSchedule {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PersonaScheduleError> {
        self.validate_syntax()?;
        let mut bytes = canonical_json_bytes(
            &serde_json::to_value(self)
                .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?,
        )
        .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn digest(&self) -> Result<String, PersonaScheduleError> {
        Ok(hash_bytes(&self.canonical_bytes()?))
    }

    pub fn parse_canonical(plan: &PersonaPlan, bytes: &[u8]) -> Result<Self, PersonaScheduleError> {
        preflight_json(bytes, "schedule")?;
        let schedule: Self =
            serde_json::from_slice(bytes).map_err(|e| PersonaScheduleError::Json(e.to_string()))?;
        if schedule.canonical_bytes()? != bytes {
            return Err(PersonaScheduleError::NonCanonical);
        }
        schedule.validate_against_plan(plan)?;
        Ok(schedule)
    }

    fn validate_syntax(&self) -> Result<(), PersonaScheduleError> {
        if self.schema != SCHEMA || !valid_digest(&self.plan_digest) {
            return Err(PersonaScheduleError::Invalid(
                "schema or plan digest".into(),
            ));
        }
        if self.events.len() > MAX_EVENTS_PER_PERSON {
            return Err(PersonaScheduleError::Invalid("event capacity".into()));
        }
        let mut ids = BTreeSet::new();
        let mut items = BTreeSet::new();
        let mut prior = None;
        let mut event_index = BTreeMap::new();
        let mut previous_wave = None;
        let mut w5_auto_seen = false;
        let mut w5_purge_seen = false;
        for (index, event) in self.events.iter().enumerate() {
            if event.event_id.is_empty()
                || event.item_id.is_empty()
                || !event
                    .event_id
                    .starts_with(&format!("{}-", self.persona_id.as_str()))
                || !ids.insert(&event.event_id)
                || !items.insert(&event.item_id)
            {
                return Err(PersonaScheduleError::Invalid(
                    "duplicate or empty event/item id".into(),
                ));
            }
            let expected_tick = u32::try_from(index + 1)
                .map_err(|_| PersonaScheduleError::Invalid("logical tick overflow".into()))?;
            if event.logical_tick != expected_tick
                || event.timestamp != format!("T{expected_tick:08}")
                || event.prior_item_id != prior
            {
                return Err(PersonaScheduleError::Invalid(
                    "noncontiguous prior chain".into(),
                ));
            }
            let wave = wave_number(event.wave);
            if previous_wave.is_some_and(|previous| wave < previous) {
                return Err(PersonaScheduleError::Invalid("wave order".into()));
            }
            if previous_wave != Some(wave) {
                w5_auto_seen = false;
                w5_purge_seen = false;
            }
            previous_wave = Some(wave);
            if event.wave == Wave::W5 {
                match event.phase {
                    EventPhase::IndexAuto if w5_purge_seen => {
                        return Err(PersonaScheduleError::Invalid(
                            "W5 auto index after purge".into(),
                        ));
                    }
                    EventPhase::IndexAuto => w5_auto_seen = true,
                    EventPhase::Regular if w5_auto_seen && event.operation != Operation::Purge => {
                        return Err(PersonaScheduleError::Invalid(
                            "W5 regular event after auto index".into(),
                        ));
                    }
                    EventPhase::Purge
                        if event.operation == Operation::Purge
                            && event.boundary == Boundary::None
                            && !w5_auto_seen =>
                    {
                        return Err(PersonaScheduleError::Invalid(
                            "W5 purge before auto index".into(),
                        ));
                    }
                    EventPhase::Purge
                        if event.operation == Operation::Purge
                            && event.boundary == Boundary::None =>
                    {
                        w5_purge_seen = true
                    }
                    EventPhase::IndexNoop if !w5_purge_seen => {
                        return Err(PersonaScheduleError::Invalid(
                            "W5 boundary before purge".into(),
                        ));
                    }
                    _ => {}
                }
            }
            let mut dependency_ids = BTreeSet::new();
            for dependency in &event.depends_on {
                if !dependency_ids.insert(dependency) {
                    return Err(PersonaScheduleError::Invalid(
                        "dependency is not earlier and unique".into(),
                    ));
                }
                let dep = event_index.get(dependency).ok_or_else(|| {
                    PersonaScheduleError::Invalid("dependency is not earlier and unique".into())
                })?;
                if *dep >= index {
                    return Err(PersonaScheduleError::Invalid("forward dependency".into()));
                }
            }
            match event.phase {
                EventPhase::Regular => {
                    if event.boundary != Boundary::None {
                        return Err(PersonaScheduleError::Invalid("regular boundary".into()));
                    }
                }
                EventPhase::IndexAuto => {
                    if event.operation != Operation::Index || event.boundary != Boundary::IndexAuto
                    {
                        return Err(PersonaScheduleError::Invalid("auto index shape".into()));
                    }
                }
                EventPhase::Purge if event.boundary == Boundary::PurgedCommit => {
                    if event.operation != Operation::Purge
                        || event.boundary != Boundary::PurgedCommit
                        || event.versions != [0, 1]
                    {
                        return Err(PersonaScheduleError::Invalid("purge shape".into()));
                    }
                }
                EventPhase::Purge
                    if event.operation == Operation::Purge && event.boundary == Boundary::None => {}
                EventPhase::Purge => {
                    return Err(PersonaScheduleError::Invalid("purge phase shape".into()));
                }
                EventPhase::IndexNoop => {
                    if event.operation != Operation::Index || event.boundary != Boundary::IndexNoop
                    {
                        return Err(PersonaScheduleError::Invalid("noop index shape".into()));
                    }
                }
            }
            match (event.origin, event.history_action) {
                (EventOrigin::History, Some(HistoryAction::Edit))
                    if event.operation == Operation::Edit
                        && event.old_planned_identity.is_none()
                        && event.new_planned_identity.is_none()
                        && event.purged_version_identities.is_empty() => {}
                (EventOrigin::History, Some(HistoryAction::DeleteAndReplace))
                    if event.operation == Operation::Delete
                        && event
                            .old_planned_identity
                            .as_deref()
                            .is_some_and(valid_digest)
                        && event
                            .new_planned_identity
                            .as_deref()
                            .is_some_and(valid_digest)
                        && event.old_planned_identity != event.new_planned_identity => {}
                (EventOrigin::History, Some(HistoryAction::CreateReplacement))
                    if event.operation == Operation::Create
                        && event.old_planned_identity.is_none()
                        && event
                            .new_planned_identity
                            .as_deref()
                            .is_some_and(valid_digest) => {}
                (EventOrigin::History, Some(HistoryAction::Purge))
                    if event.operation == Operation::Purge
                        && event.purged_version_identities.len() == 2
                        && event
                            .purged_version_identities
                            .iter()
                            .all(|value| valid_digest(value))
                        && event.purged_version_identities[0]
                            != event.purged_version_identities[1] => {}
                (EventOrigin::History, _) => {
                    return Err(PersonaScheduleError::Invalid("history action shape".into()));
                }
                (_, None)
                    if event.old_planned_identity.is_none()
                        && event.new_planned_identity.is_none()
                        && event.purged_version_identities.is_empty() => {}
                _ => {
                    return Err(PersonaScheduleError::Invalid(
                        "non-history action shape".into(),
                    ));
                }
            }
            if event.boundary != Boundary::PurgedCommit && !event.versions.is_empty() {
                return Err(PersonaScheduleError::Invalid(
                    "versions outside purge".into(),
                ));
            }
            prior = Some(event.item_id.clone());
            event_index.insert(event.event_id.clone(), index);
        }
        let mut last_commit_by_scope = BTreeMap::new();
        for pair in self.events.windows(2) {
            if pair[1].phase == EventPhase::Purge && pair[1].boundary == Boundary::PurgedCommit {
                if pair[0].phase != EventPhase::Purge
                    || pair[0].boundary != Boundary::None
                    || pair[0].history_action != Some(HistoryAction::Purge)
                    || pair[1].wave != Wave::W5
                    || pair[1].depends_on.as_slice() != [pair[0].event_id.clone()]
                {
                    return Err(PersonaScheduleError::Invalid(
                        "W5 purge/commit order".into(),
                    ));
                }
                let scope = pair[1]
                    .scope_id
                    .clone()
                    .ok_or_else(|| PersonaScheduleError::Invalid("purge scope".into()))?;
                last_commit_by_scope.insert(scope, pair[1].event_id.clone());
            }
        }
        let noops: BTreeMap<_, _> = self
            .events
            .iter()
            .filter(|event| event.phase == EventPhase::IndexNoop)
            .map(|event| {
                (
                    event.scope_id.clone().unwrap_or_default(),
                    event.depends_on.clone(),
                )
            })
            .collect();
        if noops.len() != last_commit_by_scope.len()
            || last_commit_by_scope.iter().any(|(scope, commit)| {
                noops
                    .get(scope)
                    .is_none_or(|dependencies| dependencies.as_slice() != [commit.clone()])
            })
        {
            return Err(PersonaScheduleError::Invalid("W5 scope noop set".into()));
        }
        Ok(())
    }

    /// Rebuild from the sole plan authority.  A self-consistent schedule is
    /// not accepted unless every event and identity exactly matches it.
    pub fn validate_against_plan(&self, plan: &PersonaPlan) -> Result<(), PersonaScheduleError> {
        self.validate_syntax()?;
        plan.validate()
            .map_err(|error| PersonaScheduleError::Plan(error.to_string()))?;
        let person = plan
            .personas
            .iter()
            .find(|person| person.id == self.persona_id)
            .ok_or_else(|| PersonaScheduleError::Invalid("persona absent from plan".into()))?;
        if self.plan_digest
            != plan
                .digest()
                .map_err(|error| PersonaScheduleError::Plan(error.to_string()))?
        {
            return Err(PersonaScheduleError::Invalid("plan digest binding".into()));
        }
        let expected = build_person_schedule(plan, person)?;
        if self != &expected {
            return Err(PersonaScheduleError::Invalid(
                "schedule differs from plan rebuild".into(),
            ));
        }
        Ok(())
    }
}

impl SuiteSchedule {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PersonaScheduleError> {
        self.validate_syntax()?;
        let mut bytes = canonical_json_bytes(
            &serde_json::to_value(self)
                .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?,
        )
        .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
    pub fn parse_canonical(plan: &PersonaPlan, bytes: &[u8]) -> Result<Self, PersonaScheduleError> {
        preflight_json(bytes, "suite")?;
        let suite: Self =
            serde_json::from_slice(bytes).map_err(|e| PersonaScheduleError::Json(e.to_string()))?;
        if suite.canonical_bytes()? != bytes {
            return Err(PersonaScheduleError::NonCanonical);
        }
        suite.validate_against_plan(plan)?;
        Ok(suite)
    }
    fn validate_syntax(&self) -> Result<(), PersonaScheduleError> {
        if self.schema != SCHEMA || !valid_digest(&self.plan_digest) || self.people.len() != 20 {
            return Err(PersonaScheduleError::Invalid(
                "schema or person count".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        let mut previous = None;
        for summary in &self.people {
            if !ids.insert(summary.persona_id)
                || previous.is_some_and(|id| summary.persona_id <= id)
                || summary.event_count == 0
                || !valid_digest(&summary.schedule_digest)
                || !valid_digest(&summary.projection_digest)
            {
                return Err(PersonaScheduleError::Invalid("invalid summary".into()));
            }
            previous = Some(summary.persona_id);
        }
        let mut event_ids = BTreeSet::new();
        let mut item_ids = BTreeSet::new();
        let mut prior = None;
        let mut ordering = None;
        if self.projections.len() > MAX_SUITE_PROJECTIONS {
            return Err(PersonaScheduleError::Invalid(
                "suite projection capacity".into(),
            ));
        }
        for (index, projection) in self.projections.iter().enumerate() {
            let ordinal = u32::try_from(index + 1)
                .map_err(|_| PersonaScheduleError::Invalid("suite ordinal".into()))?;
            let key = (
                wave_number(projection.wave),
                projection.phase,
                projection.persona_id,
                projection.local_tick,
            );
            if projection.suite_ordinal != ordinal
                || projection.prior_item_id != prior
                || ordering.is_some_and(|previous| key < previous)
                || !event_ids.insert(&projection.event_id)
                || !item_ids.insert(&projection.item_id)
                || !valid_digest(&projection.planned_identity)
            {
                return Err(PersonaScheduleError::Invalid(
                    "suite projection chain or uniqueness".into(),
                ));
            }
            ordering = Some(key);
            prior = Some(projection.item_id.clone());
        }
        let expected = suite_digest(&self.plan_digest, &self.people, &self.projections)?;
        if self.suite_digest != expected {
            return Err(PersonaScheduleError::Invalid("suite digest".into()));
        }
        Ok(())
    }
    pub fn validate_against_plan(&self, plan: &PersonaPlan) -> Result<(), PersonaScheduleError> {
        self.validate_syntax()?;
        let expected = build_suite_schedule(plan)?;
        if self != &expected {
            return Err(PersonaScheduleError::Invalid(
                "suite differs from plan rebuild".into(),
            ));
        }
        Ok(())
    }
}

/// Expand one bounded person at a time.  The plan remains the only inventory
/// authority; all IDs are derived from its source and structural rows.
pub fn build_person_schedule(
    plan: &PersonaPlan,
    person: &PersonPlan,
) -> Result<PersonSchedule, PersonaScheduleError> {
    plan.validate()
        .map_err(|e| PersonaScheduleError::Plan(e.to_string()))?;
    if !plan
        .personas
        .iter()
        .any(|candidate| candidate.id == person.id)
    {
        return Err(PersonaScheduleError::Plan(
            "person is not owned by plan".into(),
        ));
    }
    let plan_digest = plan
        .digest()
        .map_err(|e| PersonaScheduleError::Plan(e.to_string()))?;
    build_person_schedule_validated(person, &plan_digest)
}

/// Internal expansion used by the suite builder after the plan and digest have
/// already been established.  Keeping this separate prevents an O(people)
/// repetition of whole-plan validation and canonical hashing.
fn build_person_schedule_validated(
    person: &PersonPlan,
    plan_digest: &str,
) -> Result<PersonSchedule, PersonaScheduleError> {
    let sources =
        source_projections(person).map_err(|e| PersonaScheduleError::Plan(e.to_string()))?;
    let mut events = Vec::new();
    let mut current = BTreeMap::new();
    for source in &sources {
        current.insert(source.source_id.as_str(), source);
    }
    for wave in [Wave::W1, Wave::W2, Wave::W3, Wave::W4, Wave::W5] {
        let start = events.len();
        for source in &sources {
            let Some(cohort) = source.cohort else {
                continue;
            };
            let op = history_operation(wave, cohort);
            if let Some(operation) = op {
                push_regular(
                    &mut events,
                    format!(
                        "{}-w{}-history-{}",
                        person.id.as_str(),
                        wave_number(wave),
                        source.source_id
                    ),
                    operation,
                    source.source_id.clone(),
                    Some(source.scope_id.clone()),
                    Some(cohort),
                    None,
                    vec![],
                )?;
                let event = events.last_mut().expect("push_regular must append");
                let action = history_action(wave, cohort).expect("history operation has action");
                event.history_action = Some(action);
                event.current_chunks_delta = 0;
                event.history_chunks_delta = chunk_delta(source.planned_chunks)?;
                if action == HistoryAction::DeleteAndReplace {
                    event.old_planned_identity =
                        Some(version_identity(plan_digest, &source.source_id, "w4-v0"));
                    event.new_planned_identity =
                        Some(version_identity(plan_digest, &source.source_id, "w4-v1"));
                }
            }
        }
        for structural in person.structural.iter().filter(|row| row.wave == wave) {
            // Create/derived rows introduce plan-owned source IDs which are
            // deliberately absent from the initial source projection.
            let source = current.get(structural.source_id.as_str());
            let operation = structural_operation(structural.kind);
            push_regular(
                &mut events,
                structural.event_id.clone(),
                operation,
                structural.source_id.clone(),
                structural
                    .destination_scope_id
                    .clone()
                    .or_else(|| structural.source_scope_id.clone())
                    .or_else(|| source.map(|source| source.scope_id.clone())),
                source.and_then(|source| source.cohort),
                structural.paired_event_id.clone(),
                structural.depends_on.clone(),
            )?;
            events
                .last_mut()
                .expect("structural event was appended")
                .physical_files_delta = match structural.kind {
                StructuralKind::Create
                | StructuralKind::ExactDuplicate
                | StructuralKind::NearDuplicate
                | StructuralKind::DerivedFormat
                | StructuralKind::RestoreToActiveScope => 1,
                StructuralKind::DeleteForRestore => -1,
                _ => 0,
            };
        }
        if wave == Wave::W5 {
            // P replacements are explicit regular creates.  Each is paired to
            // its immediately following purge, so current/history arithmetic is
            // neutral after the purge commits.
            for source in sources
                .iter()
                .filter(|source| source.cohort == Some(Cohort::P))
            {
                let replacement = format!(
                    "{}-w5-history-replacement-{}",
                    person.id.as_str(),
                    source.source_id
                );
                let purge = format!(
                    "{}-w5-history-path-purge-{}",
                    person.id.as_str(),
                    source.source_id
                );
                push_regular(
                    &mut events,
                    replacement,
                    Operation::Create,
                    format!("{}-replacement", source.source_id),
                    Some(source.scope_id.clone()),
                    Some(Cohort::P),
                    Some(purge),
                    vec![],
                )?;
                let event = events.last_mut().expect("replacement was appended");
                event.history_action = Some(HistoryAction::CreateReplacement);
                event.current_chunks_delta = chunk_delta(source.planned_chunks)?;
                event.history_chunks_delta = 0;
                event.physical_files_delta = 1;
                event.new_planned_identity = Some(version_identity(
                    plan_digest,
                    &source.source_id,
                    "w5-replacement",
                ));
            }
        }
        let affected: BTreeSet<_> = events[start..]
            .iter()
            .filter_map(|event| event.scope_id.clone())
            .collect();
        for scope in affected {
            push_boundary(
                &mut events,
                person.id,
                wave,
                EventPhase::IndexAuto,
                Boundary::IndexAuto,
                Some(scope),
                None,
                vec![],
            )?;
        }
        if wave == Wave::W5 {
            let mut last_commit_by_scope = BTreeMap::new();
            for source in sources
                .iter()
                .filter(|source| source.cohort == Some(Cohort::P))
            {
                let purge = format!(
                    "{}-w5-history-path-purge-{}",
                    person.id.as_str(),
                    source.source_id
                );
                let replacement = format!(
                    "{}-w5-history-replacement-{}",
                    person.id.as_str(),
                    source.source_id
                );
                push_regular(
                    &mut events,
                    purge.clone(),
                    Operation::Purge,
                    source.source_id.clone(),
                    Some(source.scope_id.clone()),
                    Some(Cohort::P),
                    Some(replacement),
                    vec![],
                )?;
                let event = events.last_mut().expect("purge was appended");
                event.history_action = Some(HistoryAction::Purge);
                let delta = chunk_delta(source.planned_chunks)?;
                event.current_chunks_delta = delta
                    .checked_neg()
                    .ok_or_else(|| PersonaScheduleError::Invalid("chunk delta negation".into()))?;
                event.history_chunks_delta = event.current_chunks_delta;
                event.physical_files_delta = -1;
                event.purged_version_identities = vec![
                    version_identity(plan_digest, &source.source_id, "w5-v0"),
                    version_identity(plan_digest, &source.source_id, "w5-v1"),
                ];
                let commit = format!("{purge}-commit");
                push_event(
                    &mut events,
                    ScheduleEvent {
                        event_id: commit.clone(),
                        item_id: String::new(),
                        logical_tick: 0,
                        timestamp: String::new(),
                        wave,
                        phase: EventPhase::Purge,
                        origin: EventOrigin::Boundary,
                        history_action: None,
                        operation: Operation::Purge,
                        boundary: Boundary::PurgedCommit,
                        source_id: Some(source.source_id.clone()),
                        scope_id: Some(source.scope_id.clone()),
                        cohort: Some(Cohort::P),
                        depends_on: vec![purge],
                        prior_item_id: None,
                        versions: vec![0, 1],
                        paired_event_id: None,
                        old_planned_identity: None,
                        new_planned_identity: None,
                        purged_version_identities: vec![],
                        current_chunks_delta: 0,
                        history_chunks_delta: 0,
                        physical_files_delta: 0,
                    },
                )?;
                last_commit_by_scope.insert(source.scope_id.clone(), commit);
            }
            for (scope, commit) in last_commit_by_scope {
                push_boundary(
                    &mut events,
                    person.id,
                    wave,
                    EventPhase::IndexNoop,
                    Boundary::IndexNoop,
                    Some(scope),
                    Some(commit),
                    vec![],
                )?;
            }
        }
    }
    let schedule = PersonSchedule {
        schema: SCHEMA.into(),
        persona_id: person.id,
        plan_digest: plan_digest.to_owned(),
        events,
    };
    schedule.validate_syntax()?;
    validate_person_against_plan(&schedule, person, &sources)?;
    Ok(schedule)
}

/// Build suite metadata by streaming person schedules.  Full event lists are
/// intentionally discarded after their compact digest/projection is emitted.
pub fn build_suite_schedule(plan: &PersonaPlan) -> Result<SuiteSchedule, PersonaScheduleError> {
    plan.validate()
        .map_err(|e| PersonaScheduleError::Plan(e.to_string()))?;
    let plan_digest = plan
        .digest()
        .map_err(|e| PersonaScheduleError::Plan(e.to_string()))?;
    let mut people = Vec::with_capacity(plan.personas.len());
    let mut projections = Vec::new();
    for person in &plan.personas {
        let schedule = build_person_schedule_validated(person, &plan_digest)?;
        let projection = projection_digest(&schedule)?;
        for event in &schedule.events {
            let suite_ordinal = u32::try_from(projections.len() + 1)
                .map_err(|_| PersonaScheduleError::Invalid("suite projection capacity".into()))?;
            let prior_item_id = projections
                .last()
                .map(|row: &SuiteProjection| row.item_id.clone());
            let identity = hash_bytes(
                &canonical_json_bytes(
                    &serde_json::to_value(event)
                        .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?,
                )
                .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?,
            );
            projections.push(SuiteProjection {
                suite_ordinal,
                persona_id: person.id,
                local_tick: event.logical_tick,
                event_id: event.event_id.clone(),
                item_id: format!("suite-item-{suite_ordinal:08}"),
                wave: event.wave,
                phase: event.phase,
                planned_identity: identity,
                prior_item_id,
            });
        }
        people.push(PersonScheduleSummary {
            persona_id: person.id,
            event_count: u32::try_from(schedule.events.len())
                .map_err(|_| PersonaScheduleError::Invalid("event count".into()))?,
            schedule_digest: schedule.digest()?,
            projection_digest: projection,
        });
    }
    projections.sort_by_key(|row| {
        (
            wave_number(row.wave),
            row.phase,
            row.persona_id,
            row.local_tick,
        )
    });
    for index in 0..projections.len() {
        let ordinal = u32::try_from(index + 1)
            .map_err(|_| PersonaScheduleError::Invalid("suite projection capacity".into()))?;
        projections[index].suite_ordinal = ordinal;
        projections[index].item_id = format!("suite-item-{ordinal:08}");
        projections[index].prior_item_id = index
            .checked_sub(1)
            .map(|previous| projections[previous].item_id.clone());
    }
    let suite_digest = suite_digest(&plan_digest, &people, &projections)?;
    let suite = SuiteSchedule {
        schema: SCHEMA.into(),
        plan_digest,
        people,
        projections,
        suite_digest,
    };
    suite.validate_syntax()?;
    Ok(suite)
}

// Event construction stays adjacent to the expansion loop; the arguments are
// the plan columns required to preserve a structural/history row verbatim.
#[allow(clippy::too_many_arguments)]
fn push_regular(
    events: &mut Vec<ScheduleEvent>,
    event_id: String,
    operation: Operation,
    source_id: String,
    scope_id: Option<String>,
    cohort: Option<Cohort>,
    paired: Option<String>,
    depends_on: Vec<String>,
) -> Result<(), PersonaScheduleError> {
    let wave = wave_from_id(&event_id)?;
    let origin = if event_id.contains("-history-") {
        EventOrigin::History
    } else {
        EventOrigin::Structural
    };
    push_event(
        events,
        ScheduleEvent {
            event_id,
            item_id: String::new(),
            logical_tick: 0,
            timestamp: String::new(),
            wave,
            phase: if operation == Operation::Purge {
                EventPhase::Purge
            } else {
                EventPhase::Regular
            },
            origin,
            history_action: None,
            operation,
            boundary: Boundary::None,
            source_id: Some(source_id),
            scope_id,
            cohort,
            depends_on,
            prior_item_id: None,
            versions: vec![],
            paired_event_id: paired,
            old_planned_identity: None,
            new_planned_identity: None,
            purged_version_identities: vec![],
            current_chunks_delta: 0,
            history_chunks_delta: 0,
            physical_files_delta: 0,
        },
    )
}
#[allow(clippy::too_many_arguments)]
fn push_boundary(
    events: &mut Vec<ScheduleEvent>,
    persona_id: PersonaId,
    wave: Wave,
    phase: EventPhase,
    boundary: Boundary,
    scope_id: Option<String>,
    dependency: Option<String>,
    mut depends_on: Vec<String>,
) -> Result<(), PersonaScheduleError> {
    if let Some(dep) = dependency {
        depends_on.push(dep);
    }
    let ordinal = events.len() + 1;
    push_event(
        events,
        ScheduleEvent {
            event_id: format!(
                "{}-boundary-w{}-{}-{:06}",
                persona_id.as_str(),
                wave_number(wave),
                match phase {
                    EventPhase::IndexAuto => "auto",
                    EventPhase::IndexNoop => "noop",
                    _ => "invalid",
                },
                ordinal
            ),
            item_id: String::new(),
            logical_tick: 0,
            timestamp: String::new(),
            wave,
            phase,
            origin: EventOrigin::Boundary,
            history_action: None,
            operation: Operation::Index,
            boundary,
            source_id: None,
            scope_id,
            cohort: None,
            depends_on,
            prior_item_id: None,
            versions: vec![],
            paired_event_id: None,
            old_planned_identity: None,
            new_planned_identity: None,
            purged_version_identities: vec![],
            current_chunks_delta: 0,
            history_chunks_delta: 0,
            physical_files_delta: 0,
        },
    )
}
fn push_event(
    events: &mut Vec<ScheduleEvent>,
    mut event: ScheduleEvent,
) -> Result<(), PersonaScheduleError> {
    let tick = u32::try_from(events.len() + 1)
        .map_err(|_| PersonaScheduleError::Invalid("event tick overflow".into()))?;
    event.logical_tick = tick;
    event.timestamp = format!("T{tick:08}");
    event.item_id = format!("item-{tick:08}");
    event.prior_item_id = events.last().map(|prior| prior.item_id.clone());
    events.push(event);
    Ok(())
}

fn history_operation(wave: Wave, cohort: Cohort) -> Option<Operation> {
    match (wave, cohort) {
        (Wave::W1, Cohort::P | Cohort::X | Cohort::Y)
        | (Wave::W3, Cohort::X | Cohort::Y | Cohort::N)
        | (Wave::W5, Cohort::N) => Some(Operation::Edit),
        (Wave::W4, Cohort::X) => Some(Operation::Delete),
        _ => None,
    }
}
fn history_action(wave: Wave, cohort: Cohort) -> Option<HistoryAction> {
    history_operation(wave, cohort).map(|_| {
        if wave == Wave::W4 && cohort == Cohort::X {
            HistoryAction::DeleteAndReplace
        } else {
            HistoryAction::Edit
        }
    })
}
fn structural_operation(kind: StructuralKind) -> Operation {
    match kind {
        StructuralKind::SameScopeRename
        | StructuralKind::CrossScopeMove
        | StructuralKind::ArchiveMove => Operation::Move,
        StructuralKind::Create => Operation::Create,
        StructuralKind::ExactDuplicate | StructuralKind::NearDuplicate => Operation::Duplicate,
        StructuralKind::DerivedFormat => Operation::Derive,
        StructuralKind::DeleteForRestore => Operation::Delete,
        StructuralKind::RestoreToActiveScope => Operation::Restore,
    }
}
fn wave_number(wave: Wave) -> u8 {
    match wave {
        Wave::W0 => 0,
        Wave::W1 => 1,
        Wave::W2 => 2,
        Wave::W3 => 3,
        Wave::W4 => 4,
        Wave::W5 => 5,
    }
}
fn wave_from_id(id: &str) -> Result<Wave, PersonaScheduleError> {
    if id.contains("-w1-") || id.contains("-W1-") {
        Ok(Wave::W1)
    } else if id.contains("-w2-") || id.contains("-W2-") {
        Ok(Wave::W2)
    } else if id.contains("-w3-") || id.contains("-W3-") {
        Ok(Wave::W3)
    } else if id.contains("-w4-") || id.contains("-W4-") {
        Ok(Wave::W4)
    } else if id.contains("-w5-") || id.contains("-W5-") {
        Ok(Wave::W5)
    } else {
        Err(PersonaScheduleError::Invalid("event id has no wave".into()))
    }
}
fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn chunk_delta(chunks: u32) -> Result<i32, PersonaScheduleError> {
    i32::try_from(chunks)
        .map_err(|_| PersonaScheduleError::Invalid("chunk delta conversion".into()))
}

/// Refuse oversized/deep JSON before asking serde to allocate nested values.
/// This is deliberately a lexical bound rather than a second JSON parser;
/// serde remains responsible for JSON grammar and type validation.
fn preflight_json(bytes: &[u8], label: &str) -> Result<(), PersonaScheduleError> {
    if bytes.len() > MAX_CANONICAL_BYTES {
        return Err(PersonaScheduleError::Invalid(format!("{label} byte bound")));
    }
    let mut depth = 0usize;
    let mut string_len = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let (max_container_elements, max_tokens, max_objects, max_arrays) = if label == "suite" {
        (
            MAX_SUITE_PROJECTIONS,
            MAX_SUITE_PROJECTIONS
                .checked_mul(16)
                .ok_or_else(|| PersonaScheduleError::Invalid("suite lexical capacity".into()))?,
            MAX_SUITE_PROJECTIONS
                .checked_mul(2)
                .ok_or_else(|| PersonaScheduleError::Invalid("suite object capacity".into()))?,
            MAX_SUITE_PROJECTIONS
                .checked_add(64)
                .ok_or_else(|| PersonaScheduleError::Invalid("suite array capacity".into()))?,
        )
    } else {
        (
            MAX_EVENTS_PER_PERSON,
            MAX_EVENTS_PER_PERSON
                .checked_mul(32)
                .ok_or_else(|| PersonaScheduleError::Invalid("schedule lexical capacity".into()))?,
            MAX_EVENTS_PER_PERSON
                .checked_mul(4)
                .ok_or_else(|| PersonaScheduleError::Invalid("schedule object capacity".into()))?,
            MAX_EVENTS_PER_PERSON
                .checked_add(64)
                .ok_or_else(|| PersonaScheduleError::Invalid("schedule array capacity".into()))?,
        )
    };
    let mut containers: Vec<(u8, usize)> = Vec::new();
    let mut tokens = 0usize;
    let mut object_count = 0usize;
    let mut array_count = 0usize;
    for &byte in bytes {
        if in_string {
            string_len = string_len
                .checked_add(1)
                .ok_or_else(|| PersonaScheduleError::Invalid(format!("{label} string capacity")))?;
            if string_len > MAX_JSON_STRING_BYTES {
                return Err(PersonaScheduleError::Invalid(format!(
                    "{label} string bound"
                )));
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_len = 0;
            }
            b'{' | b'[' => {
                tokens = tokens.checked_add(1).ok_or_else(|| {
                    PersonaScheduleError::Invalid(format!("{label} token capacity"))
                })?;
                depth = depth.checked_add(1).ok_or_else(|| {
                    PersonaScheduleError::Invalid(format!("{label} nesting capacity"))
                })?;
                if depth > MAX_JSON_DEPTH {
                    return Err(PersonaScheduleError::Invalid(format!(
                        "{label} nesting bound"
                    )));
                }
                if byte == b'{' {
                    object_count = object_count.checked_add(1).ok_or_else(|| {
                        PersonaScheduleError::Invalid(format!("{label} object capacity"))
                    })?;
                    if object_count > max_objects {
                        return Err(PersonaScheduleError::Invalid(format!(
                            "{label} object bound"
                        )));
                    }
                } else {
                    array_count = array_count.checked_add(1).ok_or_else(|| {
                        PersonaScheduleError::Invalid(format!("{label} array capacity"))
                    })?;
                    if array_count > max_arrays {
                        return Err(PersonaScheduleError::Invalid(format!(
                            "{label} array bound"
                        )));
                    }
                }
                containers.push((byte, 0));
            }
            b'}' | b']' => {
                tokens = tokens.checked_add(1).ok_or_else(|| {
                    PersonaScheduleError::Invalid(format!("{label} token capacity"))
                })?;
                containers.pop();
                depth = depth.saturating_sub(1);
            }
            b',' => {
                tokens = tokens.checked_add(1).ok_or_else(|| {
                    PersonaScheduleError::Invalid(format!("{label} token capacity"))
                })?;
                if let Some((kind, elements)) = containers.last_mut() {
                    *elements = elements.checked_add(1).ok_or_else(|| {
                        PersonaScheduleError::Invalid(format!("{label} member capacity"))
                    })?;
                    let maximum = if *kind == b'{' {
                        MAX_OBJECT_MEMBERS
                    } else {
                        max_container_elements
                    };
                    if *elements >= maximum {
                        return Err(PersonaScheduleError::Invalid(format!(
                            "{label} container element bound"
                        )));
                    }
                }
            }
            b':' => {
                tokens = tokens.checked_add(1).ok_or_else(|| {
                    PersonaScheduleError::Invalid(format!("{label} token capacity"))
                })?;
            }
            _ => {}
        }
        if tokens > max_tokens {
            return Err(PersonaScheduleError::Invalid(format!(
                "{label} token bound"
            )));
        }
    }
    Ok(())
}
fn version_identity(plan_digest: &str, source_id: &str, version: &str) -> String {
    hash_bytes(format!("{SCHEMA}\0{plan_digest}\0{source_id}\0{version}").as_bytes())
}
fn projection_digest(schedule: &PersonSchedule) -> Result<String, PersonaScheduleError> {
    let rows: Vec<_> = schedule
        .events
        .iter()
        .map(|event| {
            (
                &event.event_id,
                event.logical_tick,
                event.wave,
                event.phase,
                event.operation,
                &event.source_id,
            )
        })
        .collect();
    let bytes = canonical_json_bytes(
        &serde_json::to_value(rows).map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?,
    )
    .map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?;
    Ok(hash_bytes(&bytes))
}
fn suite_digest(
    plan_digest: &str,
    people: &[PersonScheduleSummary],
    projections: &[SuiteProjection],
) -> Result<String, PersonaScheduleError> {
    let value = serde_json::json!({"plan_digest": plan_digest, "people": people, "projections": projections});
    let bytes =
        canonical_json_bytes(&value).map_err(|e| PersonaScheduleError::Serialize(e.to_string()))?;
    Ok(hash_bytes(&bytes))
}
fn validate_person_against_plan(
    schedule: &PersonSchedule,
    person: &PersonPlan,
    sources: &[crate::persona_plan::SourceProjection],
) -> Result<(), PersonaScheduleError> {
    let expected: BTreeSet<_> = person
        .structural
        .iter()
        .map(|event| event.event_id.as_str())
        .collect();
    let actual: BTreeSet<_> = schedule
        .events
        .iter()
        .filter(|event| event.origin == EventOrigin::Structural)
        .map(|event| event.event_id.as_str())
        .collect();
    if expected != actual {
        return Err(PersonaScheduleError::Invalid(
            "structural plan rows not consumed exactly".into(),
        ));
    }
    let p_count = sources
        .iter()
        .filter(|source| source.cohort == Some(Cohort::P))
        .count();
    let replacements = schedule
        .events
        .iter()
        .filter(|event| event.event_id.contains("-w5-history-replacement-"))
        .count();
    let purges = schedule
        .events
        .iter()
        .filter(|event| {
            event.phase == EventPhase::Purge && event.boundary == Boundary::PurgedCommit
        })
        .count();
    if replacements != p_count || purges != p_count {
        return Err(PersonaScheduleError::Invalid(
            "P replacement/purge cardinality".into(),
        ));
    }
    for purge in schedule
        .events
        .iter()
        .filter(|event| event.history_action == Some(HistoryAction::Purge))
    {
        let replacement = schedule
            .events
            .iter()
            .find(|event| event.event_id == purge.paired_event_id.clone().unwrap_or_default())
            .ok_or_else(|| {
                PersonaScheduleError::Invalid("purge has no paired replacement".into())
            })?;
        if replacement.history_action != Some(HistoryAction::CreateReplacement)
            || replacement.paired_event_id.as_deref() != Some(purge.event_id.as_str())
        {
            return Err(PersonaScheduleError::Invalid(
                "replacement/purge pair is not reciprocal".into(),
            ));
        }
        let replacement_identity = replacement
            .new_planned_identity
            .as_deref()
            .ok_or_else(|| PersonaScheduleError::Invalid("replacement identity".into()))?;
        if purge
            .purged_version_identities
            .iter()
            .any(|identity| identity == replacement_identity)
        {
            return Err(PersonaScheduleError::Invalid(
                "replacement identity reuses purged version".into(),
            ));
        }
    }
    for wave in [Wave::W1, Wave::W2, Wave::W3, Wave::W4, Wave::W5] {
        let edits = sources
            .iter()
            .filter(|source| {
                source
                    .cohort
                    .is_some_and(|cohort| history_operation(wave, cohort).is_some())
            })
            .count();
        // W5 has one regular replacement create per P source; the paired
        // path purge is deliberately in the Purge phase and is checked above.
        let expected = edits + if wave == Wave::W5 { p_count } else { 0 };
        let actual = schedule
            .events
            .iter()
            .filter(|event| {
                event.wave == wave
                    && event.phase == EventPhase::Regular
                    && event.origin == EventOrigin::History
            })
            .count();
        if actual != expected {
            return Err(PersonaScheduleError::Invalid(
                "history cohort arithmetic".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona_plan::{PersonaProfile, frozen_plan};

    #[test]
    fn deterministic_and_streaming_tiny_suite() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let a = build_suite_schedule(&plan).unwrap();
        let b = build_suite_schedule(&plan).unwrap();
        assert_eq!(
            a.suite_digest,
            "sha256:acfa8f6a9c2640b336aab14265810da3e8ef41b099cd4a7758ed0b315b7f5acd"
        );
        assert_eq!(
            build_person_schedule(&plan, &plan.personas[0])
                .unwrap()
                .digest()
                .unwrap(),
            "sha256:ef06750dcd0e7c6601753c8c54613e029aa518b8d0fe49aaea25ac03ce9659a5"
        );
        assert_eq!(a.canonical_bytes().unwrap(), b.canonical_bytes().unwrap());
        assert_eq!(a.people.len(), 20);
    }

    #[test]
    fn tiny_and_pilot_have_deterministic_plan_bound_schedules() {
        // The Full suite is generated exactly once by the explicit manual
        // contract example. Normal CI validates deterministic schedules here.
        for profile in [PersonaProfile::Tiny, PersonaProfile::Pilot] {
            let plan = frozen_plan(profile);
            let first = build_person_schedule(&plan, &plan.personas[0]).unwrap();
            let second = build_person_schedule(&plan, &plan.personas[0]).unwrap();
            assert_eq!(
                first.canonical_bytes().unwrap(),
                second.canonical_bytes().unwrap()
            );
            assert_eq!(
                PersonSchedule::parse_canonical(&plan, &first.canonical_bytes().unwrap()).unwrap(),
                first
            );
            let suite_first = build_suite_schedule(&plan).unwrap();
            let suite_second = build_suite_schedule(&plan).unwrap();
            let expected = match profile {
                PersonaProfile::Tiny => (
                    "sha256:ef06750dcd0e7c6601753c8c54613e029aa518b8d0fe49aaea25ac03ce9659a5",
                    "sha256:acfa8f6a9c2640b336aab14265810da3e8ef41b099cd4a7758ed0b315b7f5acd",
                ),
                PersonaProfile::Pilot => (
                    "sha256:ec9e1191bf8803a1b3a685d3802c2758e4e35910a12eb9b427b2670be16d4cb6",
                    "sha256:3c47897876570fea17b1d4b6dbd0a0ea9548781126034fe3ab4e649bfd75b1a3",
                ),
                PersonaProfile::Full => unreachable!("Full is manual-only"),
            };
            assert_eq!(first.digest().unwrap(), expected.0, "{profile:?} P01");
            assert_eq!(suite_first.suite_digest, expected.1, "{profile:?} suite");
            assert_eq!(
                suite_first.canonical_bytes().unwrap(),
                suite_second.canonical_bytes().unwrap()
            );
        }
    }
    #[test]
    fn structural_and_purge_order_are_exact() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let schedule = build_person_schedule(&plan, &plan.personas[0]).unwrap();
        for event in &plan.personas[0].structural {
            assert!(
                schedule
                    .events
                    .iter()
                    .any(|row| row.event_id == event.event_id)
            );
        }
        for pair in schedule.events.windows(2) {
            if pair[1].phase == EventPhase::Purge && pair[1].boundary == Boundary::PurgedCommit {
                assert_eq!(pair[0].phase, EventPhase::Purge);
                assert_eq!(pair[0].boundary, Boundary::None);
            }
        }
    }
    #[test]
    fn tampering_fails_closed() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let mut schedule = build_person_schedule(&plan, &plan.personas[0]).unwrap();
        schedule.events[1].prior_item_id = None;
        assert!(schedule.validate_against_plan(&plan).is_err());
    }

    #[test]
    fn canonical_parse_rejects_root_pair_and_order_mutations() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let schedule = build_person_schedule(&plan, &plan.personas[0]).unwrap();
        let bytes = schedule.canonical_bytes().unwrap();
        assert_eq!(
            PersonSchedule::parse_canonical(&plan, &bytes).unwrap(),
            schedule
        );

        let mut pair = schedule.clone();
        let purge = pair
            .events
            .iter_mut()
            .find(|event| event.history_action == Some(HistoryAction::Purge))
            .unwrap();
        purge.paired_event_id = Some("p01-not-the-replacement".into());
        assert!(pair.validate_against_plan(&plan).is_err());

        let mut reordered = schedule.clone();
        reordered.events.swap(0, 1);
        assert!(reordered.validate_against_plan(&plan).is_err());

        let mut root = serde_json::to_value(&schedule).unwrap();
        root.as_object_mut()
            .unwrap()
            .insert("unknown".into(), serde_json::json!(true));
        let mut root_bytes = canonical_json_bytes(&root).unwrap();
        root_bytes.push(b'\n');
        assert!(PersonSchedule::parse_canonical(&plan, &root_bytes).is_err());
    }

    #[test]
    fn suite_parse_rejects_projection_chain_mutation() {
        let plan = frozen_plan(PersonaProfile::Tiny);
        let suite = build_suite_schedule(&plan).unwrap();
        let bytes = suite.canonical_bytes().unwrap();
        assert_eq!(
            SuiteSchedule::parse_canonical(&plan, &bytes).unwrap(),
            suite
        );
        let mut mutated = suite.clone();
        mutated.projections[1].prior_item_id = None;
        assert!(mutated.validate_against_plan(&plan).is_err());
    }

    #[test]
    fn lexical_preflight_rejects_oversized_container_before_deserialization() {
        let mut bytes = Vec::with_capacity(MAX_EVENTS_PER_PERSON + 3);
        bytes.push(b'[');
        for index in 0..=MAX_EVENTS_PER_PERSON {
            if index != 0 {
                bytes.push(b',');
            }
            bytes.push(b'0');
        }
        bytes.extend_from_slice(b"]\n");
        assert!(preflight_json(&bytes, "schedule").is_err());
    }
}
