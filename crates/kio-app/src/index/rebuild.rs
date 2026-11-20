use crate::approvals;
use crate::egress::SECRETS_TIER_B_HOLD;
use crate::embedding::plan::selected_embedding_contexts;
use crate::embedding_owners;
use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::grants;
use crate::historical_reindex::RetainedNormalizedInstance;
use crate::historical_reindex::pinned_manifest;
use crate::historical_reindex::retained_history_instances_for_roots;
use crate::image_authority::{ImageAuthorityMap, image_owner_policy};
use crate::index::chunk_ledger::ChunkPublicationEvent;
use crate::index::chunk_ledger::StoredChunk;
use crate::index::chunk_ledger::append_chunk_publication_events;
use crate::index::chunk_ledger::append_stored_chunks;
use crate::index::chunk_ledger::chunks_jsonl_path;
use crate::index::chunk_ledger::open_chunk_ledger_dir;
use crate::index::chunk_ledger::persist_chunk_object;
use crate::index::chunk_ledger::read_chunk_publication_events;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::chunk_ledger::sync_chunk_ledger_dir;
use crate::index::chunk_ledger::truncate_torn_chunk_tail;
use crate::index::chunk_ledger::visit_stored_chunks;
use crate::invocation::Invocation;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::response::skipped_units::{PublishedSkippedUnit, SkippedDisclosure, SkippedUnit};
use crate::scope_access::ensure_no_visible_purge_journal;
use crate::scope_access::is_store_not_found;
use crate::scope_access::purge_blocks_rebuild_raw;
use crate::search::projection::mark_replica_unavailable_or_log;
use crate::search::projection::write_through_projection_or_log_for_repo;
use crate::search::scope::current_embedding_policy;
use crate::settings::embedding_profile_summary;
use crate::settings::persisted_embedding_is_deterministic;
use crate::settings::{
    rebuild_embedding_entry_from_tool_lock, rebuild_embedding_profile_from_tool_lock,
};
use crate::verify_objects;
use cap_primitives::fs as cap_fs;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::CommitObject;
use kio_core::dag::NormalizeRef;
use kio_core::dag::TreeObject;
use kio_core::disk_records::{
    DiskRecords, PayloadRef, PayloadWriter, RecordPayloads, RecordWriter,
};
use kio_core::history::HistoryReader;
use kio_core::purge::PurgeState;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_index::ChunkRow;
use kio_index::TreeEntryRow;
use kio_index::chunking::ChunkingConfig;
use kio_index::chunking::ChunkingInput;
use kio_index::chunking::NormalizedUnitInput;
use kio_index::chunking::chunk_hash;
use kio_index::chunking::chunk_normalized_instance;
use kio_index::embedding_store;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::SqliteFtsIndex;
use kio_pipeline::markdownize::{
    NormalizedInstanceIdentity, NormalizedInstanceManifest, NormalizedUnitManifestEntry,
    UnitStatus, read_validated_normalized_unit, validate_normalized_manifest,
    visit_validated_normalized_units,
};
use kio_pipeline::task::EmbeddingWorkKey;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};

/// R20-10: chunk_ids whose embedding task is currently on a secrets hold (Paused
/// `secrets_tier_b_hold`). `rebuild_chunk_vec` excludes them so a content-hash twin's
/// online embedding cannot expose a held file in vector search before `--send-secrets`.
pub(crate) fn held_secret_embedding_chunk_ids(
    kio_dir: &Path,
    control: &ControlHandle,
) -> Result<BTreeSet<String>> {
    let task_store = TaskStore::new_with_control(kio_dir, control.clone());
    Ok(task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.task_type == TaskType::Embedding
                && task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
        })
        .filter_map(|task| {
            EmbeddingWorkKey::parse(&task.output_ref).map(|key| key.chunk_id().to_owned())
        })
        .collect())
}

/// Publication introductions are unit-specific, not merely `(raw, profile,
/// gen)` specific. A same-generation retry can change its pinned manifest from
/// failed to done for one unit; collapsing manifests would attach that unit to
/// an older commit that never attested it.
pub(crate) type NormalizedUnitKey = (String, String, u64, String, String);

#[derive(Debug, Clone)]
pub(crate) struct AuthenticatedNormalizedUnit {
    pub(crate) text: UnitText,
    pub(crate) introductions: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum UnitText {
    Inline(String),
    Staged {
        body: PayloadRef,
        reader: Arc<StagedUnitReader>,
    },
    Pinned {
        entry: Box<NormalizedUnitManifestEntry>,
        reader: std::sync::Arc<PinnedUnitReader>,
    },
}

#[derive(Debug)]
pub(crate) struct StagedUnitReader {
    bodies: RecordPayloads<String>,
    cached: Mutex<Option<(PayloadRef, Arc<str>)>>,
}

impl StagedUnitReader {
    fn markdown(&self, body: PayloadRef) -> Result<Arc<str>> {
        let mut cache = self
            .cached
            .lock()
            .map_err(|_| KioError::schema("staged unit reader lock poisoned"))?;
        if let Some((cached, text)) = cache.as_ref()
            && *cached == body
        {
            return Ok(text.clone());
        }
        let text: Arc<str> = self.bodies.read(body)?.into();
        *cache = Some((body, text.clone()));
        Ok(text)
    }
}

#[derive(Debug)]
pub(crate) struct PinnedUnitReader {
    store: kio_core::cas::ObjectStore,
    cached: std::sync::Mutex<Option<(String, std::sync::Arc<str>)>>,
}

impl PinnedUnitReader {
    pub(crate) fn new(repo: &Repository) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            store: repo.object_store(),
            cached: std::sync::Mutex::new(None),
        })
    }

    fn markdown(
        &self,
        row: &ChunkRow,
        entry: &NormalizedUnitManifestEntry,
    ) -> Result<std::sync::Arc<str>> {
        let mut cache = self
            .cached
            .lock()
            .map_err(|_| KioError::schema("pinned unit reader lock poisoned"))?;
        let pin = entry.unit_object_hash.as_ref().expect("validated unit pin");
        if let Some((hash, markdown)) = cache.as_ref()
            && hash == pin
        {
            return Ok(markdown.clone());
        }
        let identity = NormalizedInstanceIdentity {
            raw_hash: row.raw_hash.clone(),
            tool_profile_hash: row.tool_profile_hash.clone(),
            r#gen: row.r#gen,
        };
        let unit = read_validated_normalized_unit(&self.store, &identity, entry)
            .map_err(pipeline_to_kio)?;
        if kio_core::cas::hash_bytes(unit.markdown.as_bytes()) != row.unit_content_hash {
            return Err(KioError::schema(
                "pinned normalized unit content hash does not match chunk row",
            ));
        }
        let markdown: std::sync::Arc<str> = unit.markdown.into();
        *cache = Some((pin.clone(), markdown.clone()));
        Ok(markdown)
    }
}

pub(crate) type AuthenticatedNormalizedUnits =
    BTreeMap<NormalizedUnitKey, AuthenticatedNormalizedUnit>;

pub(crate) fn retained_unit_introductions(
    repo: &Repository,
    retained_instances: &[RetainedNormalizedInstance],
) -> Result<AuthenticatedNormalizedUnits> {
    let mut units = AuthenticatedNormalizedUnits::new();
    let reader = PinnedUnitReader::new(repo);
    for instance in retained_instances {
        let loaded = append_pinned_unit_authorities(
            repo,
            &instance.raw_hash,
            &instance.normalize,
            &instance.introductions,
            &reader,
            &mut units,
        );
        if let Err(error) = loaded {
            if verify_objects::purge_explains_missing_retained_instance(
                repo,
                &instance.raw_hash,
                &instance.normalize,
            )? {
                continue;
            }
            return Err(error);
        }
    }
    Ok(units)
}

/// Rebuild reuses the unit bodies verified during preflight. References to
/// those bodies are shared across the exact manifest pins that attest them.
#[derive(Debug)]
pub(crate) struct RebuildAuthorities {
    pub(crate) roots: BTreeSet<String>,
    image_units: Option<StagedImageOwnership>,
    units: AuthenticatedNormalizedUnits,
    pub(crate) publication: PublicationAuthenticationCache,
}

type StagedImageOwnership = DiskRecords<(NormalizedInstanceCacheKey, String)>;

fn manifest_pin(raw: &str, normalize: &NormalizeRef) -> NormalizedInstanceCacheKey {
    (
        raw.to_owned(),
        normalize.tool_profile_hash.clone(),
        normalize.r#gen,
        normalize.manifest_hash.clone(),
    )
}

fn stage_rebuild_authorities(
    repo: &Repository,
    instances: &[RetainedNormalizedInstance],
    roots: BTreeSet<String>,
    image_targets: Option<&BTreeSet<String>>,
) -> Result<(RebuildAuthorities, Vec<SkippedUnit>)> {
    let store = repo.object_store();
    let stages = store.record_stages()?;
    let mut payloads = PayloadWriter::new(&stages)?;
    let mut image_stage = None;
    let mut references = BTreeMap::new();
    let mut publication = PublicationAuthenticationCache::default();
    let mut unreadable = Vec::new();
    for instance in instances {
        let pin = manifest_pin(&instance.raw_hash, &instance.normalize);
        let manifest = match pinned_manifest(repo, &instance.raw_hash, &instance.normalize) {
            Ok(manifest) => manifest,
            Err(error) => {
                if verify_objects::purge_explains_missing_retained_instance(
                    repo,
                    &instance.raw_hash,
                    &instance.normalize,
                )? {
                    continue;
                }
                return Err(error);
            }
        };
        let identity = NormalizedInstanceIdentity {
            raw_hash: pin.0.clone(),
            tool_profile_hash: pin.1.clone(),
            r#gen: pin.2,
        };
        validate_normalized_manifest(Path::new("normalized manifest"), &identity, &manifest)
            .map_err(pipeline_to_kio)?;
        let mut closure = BTreeSet::new();
        let mut read_error = None;
        let entries = manifest
            .units
            .iter()
            .filter(|entry| entry.status == UnitStatus::Done);
        for entry in entries.clone() {
            let unit = match read_validated_normalized_unit(&store, &identity, entry) {
                Ok(unit) => unit,
                Err(error) => {
                    read_error = Some(pipeline_to_kio(error));
                    break;
                }
            };
            for hash in unit
                .owned_image_hashes
                .into_iter()
                .filter(|hash| image_targets.is_none_or(|targets| targets.contains(hash)))
            {
                if image_stage.is_none() {
                    image_stage = Some(RecordWriter::new(&stages)?);
                }
                image_stage
                    .as_mut()
                    .expect("created image stage")
                    .push(&(pin.clone(), hash))?;
            }
            let hash = kio_core::cas::hash_bytes(unit.markdown.as_bytes());
            let key = (
                pin.0.clone(),
                pin.1.clone(),
                pin.2,
                unit.unit_key.clone(),
                hash.clone(),
            );
            if let std::collections::btree_map::Entry::Vacant(entry) = references.entry(key) {
                entry.insert(payloads.push(&unit.markdown)?);
            }
            closure.insert((unit.unit_key, hash));
        }
        if let Some(error) = read_error {
            if verify_objects::purge_explains_missing_retained_instance(
                repo,
                &instance.raw_hash,
                &instance.normalize,
            )? {
                continue;
            }
            if !is_rebuild_skippable_unit_error(&error) {
                return Err(error);
            }
            publication.unreadable_unit_keys.insert(
                pin.clone(),
                entries.map(|entry| entry.unit_key.clone()).collect(),
            );
            publication.unavailable_pins.insert(pin.clone());
            unreadable.push((pin, instance, error));
        } else {
            publication.pinned_unit_closures.insert(pin, closure);
        }
    }
    let mut skipped = Vec::new();
    for (pin, instance, error) in unreadable {
        // Preserve the existing fail-closed rebuild for a missing pinned unit
        // without any verified same-generation history to recover.
        let has_healthy_pin = publication
            .pinned_unit_closures
            .range((pin.0.clone(), pin.1.clone(), pin.2, String::new())..)
            .next()
            .is_some_and(|(healthy, _)| {
                (&healthy.0, &healthy.1, healthy.2) == (&pin.0, &pin.1, pin.2)
            });
        if !has_healthy_pin {
            return Err(error);
        }
        skipped.push(SkippedUnit {
            raw_hash: instance.raw_hash.clone(),
            path: instance.raw_path.clone(),
            r#gen: instance.normalize.r#gen,
            reason: error.error_code().to_owned(),
        });
    }
    let image_units = image_stage.map(RecordWriter::finish).transpose()?;
    let reader = Arc::new(StagedUnitReader {
        bodies: payloads.finish()?,
        cached: Mutex::new(None),
    });
    let mut units = AuthenticatedNormalizedUnits::new();
    for instance in instances {
        let pin = manifest_pin(&instance.raw_hash, &instance.normalize);
        if let Some(closure) = publication.pinned_unit_closures.get(&pin) {
            for (unit_key, hash) in closure {
                let key = (
                    pin.0.clone(),
                    pin.1.clone(),
                    pin.2,
                    unit_key.clone(),
                    hash.clone(),
                );
                let body = references[&key];
                units
                    .entry(key)
                    .or_insert_with(|| AuthenticatedNormalizedUnit {
                        text: UnitText::Staged {
                            body,
                            reader: reader.clone(),
                        },
                        introductions: BTreeSet::new(),
                    })
                    .introductions
                    .extend(instance.introductions.iter().cloned());
            }
        }
    }
    Ok((
        RebuildAuthorities {
            roots,
            image_units,
            units,
            publication,
        },
        skipped,
    ))
}

impl RebuildAuthorities {
    pub(crate) fn excludes(&self, instance: &RetainedNormalizedInstance) -> bool {
        self.publication
            .unavailable_pins
            .contains(&manifest_pin(&instance.raw_hash, &instance.normalize))
    }

    pub(crate) fn exact_units(
        &self,
        instance: &RetainedNormalizedInstance,
    ) -> Option<AuthenticatedNormalizedUnits> {
        let pin = manifest_pin(&instance.raw_hash, &instance.normalize);
        if self.publication.unavailable_pins.contains(&pin) {
            return None;
        }
        self.publication
            .pinned_unit_closures
            .get(&pin)
            .map(|closure| {
                closure
                    .iter()
                    .filter_map(|(unit_key, hash)| {
                        let key = (
                            pin.0.clone(),
                            pin.1.clone(),
                            pin.2,
                            unit_key.clone(),
                            hash.clone(),
                        );
                        self.units.get(&key).map(|unit| (key, unit.clone()))
                    })
                    .collect()
            })
    }

    fn replay_image_authority(
        &self,
        instances: &[RetainedNormalizedInstance],
        policy: &kio_pipeline::policy::CurrentPolicyEvaluator,
    ) -> Result<ImageAuthorityMap> {
        let mut authority = ImageAuthorityMap::default();
        let Some(stage) = &self.image_units else {
            return Ok(authority);
        };
        let mut rows = stage.iter().peekable();
        // Rows were written in the same order as the retained instances.
        for instance in instances {
            let pin = manifest_pin(&instance.raw_hash, &instance.normalize);
            let verified = self.publication.pinned_unit_closures.contains_key(&pin)
                && !self.publication.unavailable_pins.contains(&pin);
            let mut owner = None;
            while rows
                .peek()
                .is_some_and(|row| row.as_ref().map_or(true, |(key, _)| *key == pin))
            {
                let (_, hash) = rows.next().expect("peeked ownership record")?;
                if verified {
                    if owner.is_none() {
                        owner = Some(image_owner_policy(instance, policy)?);
                    }
                    authority
                        .record_owned_hash(hash, owner.as_ref().expect("evaluated image owner"))?;
                }
            }
        }
        if rows.next().transpose()?.is_some() {
            return Err(KioError::schema(
                "staged image owner has no retained manifest",
            ));
        }
        Ok(authority)
    }

    fn retain_available_introductions(&mut self, instances: &[RetainedNormalizedInstance]) {
        if self.publication.unavailable_pins.is_empty() {
            return;
        }
        for authority in self.units.values_mut() {
            authority.introductions.clear();
        }
        for instance in instances {
            let pin = manifest_pin(&instance.raw_hash, &instance.normalize);
            if self.publication.unavailable_pins.contains(&pin) {
                continue;
            }
            if let Some(closure) = self.publication.pinned_unit_closures.get(&pin) {
                for (unit_key, hash) in closure {
                    let key = (
                        pin.0.clone(),
                        pin.1.clone(),
                        pin.2,
                        unit_key.clone(),
                        hash.clone(),
                    );
                    if let Some(authority) = self.units.get_mut(&key) {
                        authority
                            .introductions
                            .extend(instance.introductions.iter().cloned());
                    }
                }
            }
        }
        self.units
            .retain(|_, authority| !authority.introductions.is_empty());
    }
}

pub(crate) fn append_pinned_unit_authorities(
    repo: &Repository,
    raw_hash: &str,
    normalize: &NormalizeRef,
    introductions: &[String],
    reader: &std::sync::Arc<PinnedUnitReader>,
    units: &mut AuthenticatedNormalizedUnits,
) -> Result<()> {
    let manifest = pinned_manifest(repo, raw_hash, normalize)?;
    let entries = manifest
        .units
        .iter()
        .map(|entry| (entry.unit_key.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    visit_validated_normalized_units(&reader.store, &manifest, |unit| {
        let key = (
            raw_hash.to_owned(),
            normalize.tool_profile_hash.clone(),
            normalize.r#gen,
            unit.unit_key.clone(),
            kio_core::cas::hash_bytes(unit.markdown.as_bytes()),
        );
        let authority = units
            .entry(key)
            .or_insert_with(|| AuthenticatedNormalizedUnit {
                text: UnitText::Pinned {
                    entry: Box::new(entries[unit.unit_key.as_str()].clone()),
                    reader: reader.clone(),
                },
                introductions: BTreeSet::new(),
            });
        authority
            .introductions
            .extend(introductions.iter().cloned());
        Ok(())
    })
    .map_err(pipeline_to_kio)?;
    Ok(())
}

pub(crate) fn unit_authorities_from_inputs(
    units: &[NormalizedUnitInput],
) -> AuthenticatedNormalizedUnits {
    units
        .iter()
        .map(|unit| {
            (
                (
                    unit.raw_hash.clone(),
                    unit.tool_profile_hash.clone(),
                    unit.r#gen,
                    unit.unit_key.clone(),
                    unit.unit_content_hash.clone(),
                ),
                AuthenticatedNormalizedUnit {
                    text: UnitText::Inline(unit.markdown.clone()),
                    introductions: BTreeSet::new(),
                },
            )
        })
        .collect()
}

/// Fail closed before a durable chunk row is appended, restored to CAS, or
/// indexed. `chunks.jsonl` is only an acceleration ledger; its complete chunk
/// content must still be attested by a pinned immutable normalized unit.
pub(crate) fn authenticate_chunk_row(
    row: &ChunkRow,
    units: &AuthenticatedNormalizedUnits,
) -> Result<()> {
    let key = (
        row.raw_hash.clone(),
        row.tool_profile_hash.clone(),
        row.r#gen,
        row.unit_key.clone(),
        row.unit_content_hash.clone(),
    );
    let unit = units.get(&key).ok_or_else(|| {
        KioError::schema("chunk row identity is not present in a pinned normalized unit")
    })?;
    let retained;
    let markdown = match &unit.text {
        UnitText::Inline(markdown) => {
            if kio_core::cas::hash_bytes(markdown.as_bytes()) != row.unit_content_hash {
                return Err(KioError::schema(
                    "pinned normalized unit content hash does not match chunk row",
                ));
            }
            markdown.as_str()
        }
        UnitText::Staged { body, reader } => {
            retained = reader.markdown(*body)?;
            retained.as_ref()
        }
        UnitText::Pinned { entry, reader } => {
            retained = reader.markdown(row, entry)?;
            retained.as_ref()
        }
    };
    let start = usize::try_from(row.byte_start)
        .map_err(|_| KioError::schema("chunk byte_start does not fit this platform"))?;
    let end = usize::try_from(row.byte_end)
        .map_err(|_| KioError::schema("chunk byte_end does not fit this platform"))?;
    if start > end
        || end > markdown.len()
        || !markdown.is_char_boundary(start)
        || !markdown.is_char_boundary(end)
    {
        return Err(KioError::schema(
            "chunk row byte range is not a UTF-8 boundary within its pinned normalized unit",
        ));
    }
    if markdown[start..end] != row.text {
        return Err(KioError::schema(
            "chunk row text is not the exact pinned normalized-unit markdown slice",
        ));
    }
    if kio_core::cas::hash_bytes(row.text.as_bytes()) != row.text_hash {
        return Err(KioError::schema(
            "chunk row text_hash does not match exact text",
        ));
    }
    if chunk_hash(row).map_err(index_to_kio)? != row.chunk_id {
        return Err(KioError::schema(
            "chunk row identity hash does not match its canonical identity fields",
        ));
    }
    Ok(())
}

/// Current refs plus authenticated publication introductions, all constrained
/// to the sole published HEAD ancestry. Ledger associations cannot publish a
/// detached CAS commit or recover a different HEAD implicitly.
fn read_creation_metadata(kio_dir: &Path) -> Result<Vec<StoredChunk>> {
    let mut entries = Vec::new();
    visit_stored_chunks(kio_dir, |mut chunk| {
        // Publication and allocation use row identities only. The text was
        // authenticated while parsing and is read again only for SQLite replay.
        chunk.row.text = String::new();
        entries.push(chunk);
        Ok(())
    })?;
    Ok(entries)
}

pub(crate) fn durable_history_roots(repo: &Repository) -> Result<BTreeSet<String>> {
    durable_history_roots_with(
        repo,
        repo.current_ref_targets()?,
        &mut PublicationAuthenticationCache::default(),
    )
}

fn durable_history_roots_with(
    repo: &Repository,
    mut roots: BTreeSet<String>,
    authentication_cache: &mut PublicationAuthenticationCache,
) -> Result<BTreeSet<String>> {
    let final_shallow_commits = final_shallow_commit_set(repo)?;
    let creations = read_creation_metadata(repo.kio_dir())?;
    let events = read_chunk_publication_events(repo.kio_dir())?;
    // Events are append-only and may name the same association repeatedly. Keep
    // the first creation record, matching the previous linear lookup semantics,
    // while making the event-to-creation join O(E log C).
    let mut creations_by_association = BTreeMap::new();
    for creation in &creations {
        creations_by_association
            .entry((
                creation.row.chunk_id.clone(),
                creation.row.chunking_config_hash.clone(),
            ))
            .or_insert(creation);
    }
    // Publication events authenticate associations, but cannot introduce a
    // different history. Prove their membership in the HEAD chain below.
    for event in &events {
        let creation = creations_by_association
            .get(&(event.chunk_id.clone(), event.chunking_config_hash.clone()))
            .copied()
            .ok_or_else(|| KioError::schema("publication event has no creation association"))?;
        if final_shallow_commits.contains(&event.introduction_commit) {
            continue;
        }
        if authenticate_publication_event_cached(
            repo,
            repo.kio_dir(),
            event,
            creation,
            authentication_cache,
        )? {
            roots.insert(event.introduction_commit.clone());
        }
    }
    repo.validate_published_history_roots(&roots)?;
    Ok(roots)
}

/// Whether a commit is in the only normal tree-absent state: a markerless,
/// strict shallow receipt exactly bound to its commit/tree.
/// This is deliberately narrower than "has a receipt": mismatches, an active
/// sweep, a receipt/tree coexistence, and every unreceipted missing tree remain
/// visible to the caller rather than weakening durable-ledger authentication.
/// Load and validate every markerless final shallow receipt once.  Callers can
/// then make repeated history/ledger decisions without re-reading mutable
/// receipt paths per row.  Any active marker disables the final state entirely:
/// recovery owns that transitional namespace.
fn final_shallow_commit_set(repo: &Repository) -> Result<BTreeSet<String>> {
    if kio_core::gc::read_active_marker_with_control(repo.kio_dir(), repo.control())?.is_some() {
        return Ok(BTreeSet::new());
    }
    Ok(kio_core::gc::GcSweepSession::bind_repository(repo)?
        .validated_final_shallow_receipts()?
        .into_keys()
        .collect())
}

fn chunk_inputs<'a>(
    store: &'a kio_core::cas::ObjectStore,
    manifest: &'a NormalizedInstanceManifest,
) -> impl Iterator<Item = Result<NormalizedUnitInput>> + 'a {
    let identity = NormalizedInstanceIdentity {
        raw_hash: manifest.raw_hash.clone(),
        tool_profile_hash: manifest.tool_profile_hash.clone(),
        r#gen: manifest.r#gen,
    };
    manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
        .map(move |entry| {
            let unit =
                read_validated_normalized_unit(store, &identity, entry).map_err(pipeline_to_kio)?;
            let unit_content_hash = kio_core::cas::hash_bytes(unit.markdown.as_bytes());
            Ok(NormalizedUnitInput {
                raw_hash: unit.raw_hash,
                tool_profile_hash: unit.tool_profile_hash,
                r#gen: unit.r#gen,
                unit_key: unit.unit_key,
                unit_content_hash,
                markdown: unit.markdown,
            })
        })
}

enum DocumentChunks {
    Ready {
        rows: DiskRecords<PayloadRef>,
        bodies: RecordPayloads<ChunkRow>,
    },
    Unavailable(KioError),
}

fn stage_document_chunks(
    repo: &Repository,
    raw_hash: &str,
    raw_path: &str,
    normalize: &NormalizeRef,
    config: &ChunkingConfig,
    first_document: &mut bool,
) -> Result<DocumentChunks> {
    let manifest = match pinned_manifest(repo, raw_hash, normalize) {
        Ok(manifest) => manifest,
        Err(error) => return Ok(DocumentChunks::Unavailable(error)),
    };
    if std::mem::take(first_document) {
        repo.control()
            .checkpoint(kio_core::durability::DurabilityPoint::BeforeChunkUnitRead)?;
    }
    let store = repo.object_store();
    let stages = store.record_stages()?;
    let mut staged = RecordWriter::new(&stages)?;
    let mut bodies = PayloadWriter::new(&stages)?;
    for unit in chunk_inputs(&store, &manifest) {
        let unit = match unit {
            Ok(unit) => unit,
            Err(error) => return Ok(DocumentChunks::Unavailable(error)),
        };
        let input = ChunkingInput {
            raw_path: raw_path.to_owned(),
            units: vec![unit],
            config: config.clone(),
            created_at: repo.control().now_utc_seconds(),
        };
        let authorities = unit_authorities_from_inputs(&input.units);
        for row in chunk_normalized_instance(input).map_err(index_to_kio)? {
            authenticate_chunk_row(&row, &authorities)?;
            staged.push(&bodies.push(&row)?)?;
        }
    }
    Ok(DocumentChunks::Ready {
        rows: staged.finish()?,
        bodies: bodies.finish()?,
    })
}

/// The source index publishes the HEAD binding in the same SQLite transaction
/// as its chunks. A retired extraction task alone cannot prove index completion.
pub(crate) fn source_index_matches_head(repo: &Repository) -> Result<bool> {
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(true);
    };
    use kio_index::fts::{ExistingSourceIndexOpenMode, open_existing_source_index_connection};
    let Ok(connection) = open_existing_source_index_connection(
        sqlite_path(repo.kio_dir()),
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    ) else {
        return Ok(false);
    };
    let indexed = connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM tree_entries WHERE commit_hash = ?1)",
            [&head],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| KioError::schema(error.to_string()))?;
    if !indexed {
        // Resuming an extraction always captures at least one direct source.
        // An empty tree has no tree_entries rows to reconcile.
        return Ok(read_head_tree_for_rebuild(repo, &head)?.entries.is_empty());
    }
    sync_chunk_ledger_dir(&open_chunk_ledger_dir(repo.kio_dir(), false)?)?;
    Ok(true)
}

pub(crate) fn rebuild_step3_index(
    ctx: &Invocation<'_>,
    repo: &Repository,
) -> Result<Step3RebuildReport> {
    ensure_no_visible_purge_journal(repo.kio_dir(), repo.control())?;
    // HEAD is the only publication authority. Tags and chunk publication
    // records may retain ancestors, but cannot supply a substitute HEAD.
    let head = repo.head_commit_hash()?;
    // Preserve the recovery contract for the current HEAD before consulting
    // ledger-derived candidates.  A forged or stale non-HEAD ledger candidate
    // still fails closed below, but it must not mask an actual shallow HEAD
    // with a raw store-not-found error.
    let head_tree = head
        .as_deref()
        .map(|head| read_head_tree_for_rebuild(repo, head))
        .transpose()?;
    let existing = read_creation_metadata(repo.kio_dir())
        .map_err(|error| annotate_index_stage(error, "read chunk ledger"))?;
    let ref_roots = repo.current_ref_targets()?;
    if ref_roots.is_empty() {
        // Validate the ledger even without a ref. A detached publication
        // cannot supply a HEAD or justify allocating any extraction stages.
        durable_history_roots_with(
            repo,
            ref_roots,
            &mut PublicationAuthenticationCache::default(),
        )
        .map_err(|error| annotate_index_stage(error, "authenticate history roots"))?;
        return Ok(Step3RebuildReport::default());
    }
    // Publication events can only keep ancestors of the published HEAD.
    // Inspect the pins in that ref graph before authenticating those events,
    // so root selection and replay agree on which exact pins can attest them.
    let retained_instances = retained_history_instances_for_roots(repo, &ref_roots)
        .map_err(|error| annotate_index_stage(error, "load retained instances"))?;
    let image_targets = rebuild_image_targets(repo)?;
    let (mut authorities, mut skipped_units) = stage_rebuild_authorities(
        repo,
        &retained_instances,
        ref_roots.clone(),
        image_targets.as_ref(),
    )
    .map_err(|error| annotate_index_stage(error, "authenticate retained units"))?;
    authorities.roots =
        durable_history_roots_with(repo, ref_roots, &mut authorities.publication)
            .map_err(|error| annotate_index_stage(error, "authenticate history roots"))?;
    let rebuild_roots = &authorities.roots;
    // R16-1/R16-4: `repair rebuild-db` (the only implemented recovery command),
    // `index`, and `reindex` all rebuild through here. A shallow HEAD (commit OR tree
    // object gone) must fail with a clear KIO-E-COMMIT-SHALLOW-001 + recovery guidance
    // — NOT the raw KIO-E-STORE-NOT-FOUND-001 that let `repair` die on the very
    // corruption it exists to recover from (R15-4 fixed reindex but missed the shared
    // rebuilder that repair uses). Placing the conversion in this shared function
    // covers all three commands at once.
    // SQLite is derived state. Rebuild every projection from the current ref
    // roots and immutable CAS, not from whichever rows happened to survive in
    // the old database. Every root was proven to belong to HEAD ancestry.
    let mut tree_rows = BTreeMap::<(String, String), TreeEntryRow>::new();
    let (graph, _) = HistoryReader::new_with_store(repo.kio_dir(), repo.object_store())
        .walk_for_roots_allow_shallowed(rebuild_roots)
        .map_err(|error| annotate_index_stage(error, "walk retained history"))?;
    for node in graph.nodes_in_visit_order() {
        for entry in &node.tree.entries {
            let entry = entry?;
            let (tool_profile_hash, r#gen, manifest_hash) =
                entry
                    .normalize
                    .as_ref()
                    .map_or((None, None, None), |normalize| {
                        (
                            Some(normalize.tool_profile_hash.clone()),
                            Some(normalize.r#gen),
                            Some(normalize.manifest_hash.clone()),
                        )
                    });
            tree_rows.insert(
                (node.commit_hash.clone(), entry.path.clone()),
                TreeEntryRow {
                    commit_hash: node.commit_hash.clone(),
                    path: entry.path.clone(),
                    raw_hash: entry.raw_hash.clone(),
                    tool_profile_hash,
                    r#gen,
                    manifest_hash,
                },
            );
        }
    }
    let tree_entries = tree_rows.into_values().collect::<Vec<_>>();
    // PC61/62 (04 §4.6, U145): deliberately NOT applied to `retained_history_instances`
    // itself — that shared function stays the untouched source both this rebuild AND
    // the embedding-task-generation path (`retained_history_chunks`) read from, per
    // its own doc comment's documented lesson (a blanket filter there regressed
    // several `step3_p0_contract.rs` history/deleted-content tests). Instead this
    // rebuild-only set below narrows WHICH retained instances may receive a brand
    // NEW `chunk_config_generations` association this pass — see the loop below.
    let retained_instance_keys = retained_instances
        .iter()
        .map(|instance| manifest_pin(&instance.raw_hash, &instance.normalize))
        .collect::<BTreeSet<_>>();
    let mut head_identity_keys = BTreeSet::new();
    let mut head_pins = BTreeSet::new();
    if let Some(tree) = &head_tree {
        for entry in &tree.entries {
            let entry = entry?;
            if let Some(normalize) = entry.normalize {
                head_pins.insert(manifest_pin(&entry.raw_hash, &normalize));
                head_identity_keys.insert((
                    entry.raw_hash,
                    normalize.tool_profile_hash,
                    normalize.r#gen,
                ));
            }
        }
    }
    let config = read_chunking_config(repo)?;
    // PC61/62: identities that already have AT LEAST ONE durable association
    // (under any config) as of the start of this rebuild. Combined with
    // `head_identity_keys` below, this distinguishes "a config change would
    // otherwise re-chunk/re-embed already-covered, HEAD-unreachable history"
    // (PC61/62's actual target — skip) from "this identity has never been
    // chunked before" (first-ever appearance, or a torn-tail loss needing
    // self-heal — still process regardless of HEAD membership, matching the
    // pre-PC61 behavior those cases already relied on).
    let existing_identity_keys = existing
        .iter()
        .map(|chunk| {
            (
                chunk.row.raw_hash.clone(),
                chunk.row.tool_profile_hash.clone(),
                chunk.row.r#gen,
            )
        })
        .collect::<BTreeSet<(String, String, u64)>>();
    // Q1: physically remove any torn trailing record from `chunks.jsonl` before the
    // append below, so the new records land on a clean `'\n'`-terminated boundary
    // instead of being welded onto the torn bytes (which would create a
    // permanently-skipped malformed line and re-brick every later rebuild).
    // read/skip alone is not self-healing; this truncation is what makes it so.
    truncate_torn_chunk_tail(repo.kio_dir())?;
    let mut known_associations = existing
        .iter()
        .map(|chunk| {
            (
                chunk.row.chunk_id.clone(),
                chunk.row.chunking_config_hash.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut chunk_rowids = existing
        .iter()
        .map(|chunk| (chunk.row.chunk_id.clone(), chunk.rowid))
        .collect::<BTreeMap<_, _>>();
    let mut next_rowid = existing.iter().map(|chunk| chunk.rowid).max().unwrap_or(0) + 1;
    let mut next_association_rowid = existing
        .iter()
        .map(|chunk| chunk.association_rowid)
        .max()
        .unwrap_or(0)
        + 1;
    let mut appended = Vec::<StoredChunk>::new();
    let mut appended_count = 0u64;
    let mut pending_publication_events = Vec::<ChunkPublicationEvent>::new();
    // R16-4: documents whose normalized units are missing/corrupt are skipped and
    // reported here rather than aborting the whole rebuild (docs/10 §7.2: "最悪
    // objects/ と refs/ が保全されていれば復旧できる" — one bad unit must not veto
    // the recovery of every healthy document). Never silent: the caller surfaces this.
    let mut first_document = true;

    // PC61/62 (04 §4.6, U145): a retained instance NOT referenced by HEAD's own
    // tree, that ALREADY has some durable association from an earlier pass (it
    // was necessarily HEAD-referenced back when that association was created —
    // this store only ever advances HEAD forward), is skipped entirely here — a
    // chunking-config change must not re-chunk/re-embed history no live tree can
    // reach (04 §4.6: "どの tree からも到達不能な chunk と embedding 課金を生む
    // だけ"). A retained instance with NO existing association at all (first-ever
    // appearance in the ledger, or a torn-tail loss needing self-heal, PC63's own
    // note that historical instances must not be dropped wholesale) is still
    // processed regardless of HEAD membership, exactly as before PC61/62.
    // Association creation is recorded once; every authenticated tree
    // introduction is staged separately below as a publication event.
    let mut publish = |chunks: DiskRecords<PayloadRef>,
                       bodies: RecordPayloads<ChunkRow>,
                       introduction: Option<&String>|
     -> Result<()> {
        let mut appended_bytes = 0usize;
        for row in &chunks {
            let row = bodies.read(row?)?;
            let body_bytes = row
                .text
                .len()
                .saturating_add(row.heading_path.iter().flatten().map(String::len).sum());
            let chunk_id = row.chunk_id.clone();
            let chunking_config_hash = row.chunking_config_hash.clone();
            let outcome = append_authenticated_chunk_association(
                repo,
                row,
                &mut known_associations,
                &mut chunk_rowids,
                &mut next_rowid,
                &mut next_association_rowid,
                &mut appended,
            )?;
            if outcome == AssociationAppend::Created
                && let Some(introduction) = introduction
            {
                pending_publication_events.push(ChunkPublicationEvent {
                    event: "publication".to_owned(),
                    chunk_id,
                    chunking_config_hash,
                    introduction_commit: introduction.clone(),
                });
            }
            if outcome == AssociationAppend::Created {
                appended_bytes = appended_bytes.saturating_add(body_bytes);
            }
            // At most one large chunk can be in flight with the small batch.
            if appended.len() == 128 || appended_bytes >= 1024 * 1024 {
                append_stored_chunks(repo.kio_dir(), &appended)
                    .map_err(|error| annotate_index_stage(error, "append chunk creations"))?;
                appended_count += appended.len() as u64;
                appended.clear();
                appended_bytes = 0;
            }
        }
        if !appended.is_empty() {
            append_stored_chunks(repo.kio_dir(), &appended)
                .map_err(|error| annotate_index_stage(error, "append chunk creations"))?;
            appended_count += appended.len() as u64;
            appended.clear();
        }
        Ok(())
    };

    let mut rebuild_document = |raw: &str,
                                path: &str,
                                normalize: &NormalizeRef,
                                introduction: Option<&String>,
                                retained: bool|
     -> Result<()> {
        let pin = manifest_pin(raw, normalize);
        if authorities.publication.unavailable_pins.contains(&pin) {
            return Ok(());
        }
        match stage_document_chunks(repo, raw, path, normalize, &config, &mut first_document)? {
            DocumentChunks::Ready { rows, bodies } => publish(rows, bodies, introduction),
            DocumentChunks::Unavailable(error) => {
                if retained
                    && verify_objects::purge_explains_missing_retained_instance(
                        repo, raw, normalize,
                    )?
                {
                    return Ok(());
                }
                if !is_rebuild_skippable_unit_error(&error) {
                    return Err(error);
                }
                authorities.publication.unavailable_pins.insert(pin);
                skipped_units.push(SkippedUnit {
                    raw_hash: raw.to_owned(),
                    path: path.to_owned(),
                    r#gen: normalize.r#gen,
                    reason: error.error_code().to_owned(),
                });
                Ok(())
            }
        }
    };
    for retained in &retained_instances {
        let identity_key = (
            retained.raw_hash.clone(),
            retained.normalize.tool_profile_hash.clone(),
            retained.normalize.r#gen,
        );
        let in_head = head_identity_keys.contains(&identity_key);
        if !in_head && existing_identity_keys.contains(&identity_key) {
            continue;
        }
        rebuild_document(
            &retained.raw_hash,
            &retained.raw_path,
            &retained.normalize,
            head.as_ref().filter(|_| {
                head_pins.contains(&manifest_pin(&retained.raw_hash, &retained.normalize))
            }),
            true,
        )?;
    }

    for entry in head_tree
        .as_ref()
        .into_iter()
        .flat_map(|tree| &tree.entries)
    {
        let entry = entry?;
        let Some(normalize) = entry.normalize else {
            continue;
        };
        if retained_instance_keys.contains(&manifest_pin(&entry.raw_hash, &normalize)) {
            continue;
        }
        rebuild_document(
            &entry.raw_hash,
            &entry.path,
            &normalize,
            head.as_ref(),
            false,
        )?;
    }

    authorities.retain_available_introductions(&retained_instances);
    // An attestation cached while selecting roots predates document staging.
    authorities.publication.attestations.clear();

    // A creation record proves only that the chunk/config association is
    // durable.  Publication is a separate fact: every retained manifest
    // introduction that authenticates this exact immutable unit must be
    // recorded after that creation fsync, including associations that already
    // existed before this rebuild.  Otherwise removing the last ref could make
    // a perfectly valid historical introduction disappear at the next SQLite
    // loss/rebuild.
    repo.control()
        .checkpoint(kio_core::durability::DurabilityPoint::AfterChunkDocumentStaging)?;
    stage_retained_unit_publication_events(
        repo,
        repo.kio_dir(),
        &mut authorities,
        &mut pending_publication_events,
    )
    .map_err(|error| annotate_index_stage(error, "stage chunk publications"))?;
    append_chunk_publication_events(repo.kio_dir(), &pending_publication_events)
        .map_err(|error| annotate_index_stage(error, "append chunk publications"))?;
    // The source SQLite `tree_entries` table (below) is the writer-side cache for
    // live-chunk resolution and local short-hash resolution. Direct
    // search uses only replica corpus rows and bindings. The former JSON projection
    // went stale after a manual snapshot and is no longer written (L3).
    rebuild_sqlite_index(
        ctx,
        repo,
        &tree_entries,
        &retained_instances,
        &mut authorities,
        &config.chunking_config_hash,
    )
    .map_err(|error| annotate_index_stage(error, "rebuild source index"))?;
    Ok(Step3RebuildReport {
        rebuilt_chunks: appended_count,
        rebuilt_tree_entries: tree_entries.len() as u64,
        skipped_units,
        enrichment_replay: if authorities.publication.unavailable_pins.is_empty() {
            None
        } else {
            Some(authorities)
        },
    })
}

fn annotate_index_stage(error: KioError, stage: &str) -> KioError {
    KioError::new(
        error.error_code(),
        format!("{stage}: {}", error.message()),
        error.context().clone(),
        error.exit_code(),
    )
}

/// Stage durable publication rows for every authenticated retained unit.  This
/// runs only after [`append_stored_chunks`] has fsynced creation rows, so an
/// event can never outlive the association it names.  `append_chunk_publication_events`
/// owns final deduplication for retry safety.
fn stage_retained_unit_publication_events(
    repo: &Repository,
    kio_dir: &Path,
    authorities: &mut RebuildAuthorities,
    pending: &mut Vec<ChunkPublicationEvent>,
) -> Result<()> {
    let RebuildAuthorities {
        units: retained_units,
        publication,
        ..
    } = authorities;
    let mut introduction_configs = BTreeMap::<String, Option<String>>::new();
    visit_stored_chunks(kio_dir, |creation| {
        let identity = (
            creation.row.raw_hash.clone(),
            creation.row.tool_profile_hash.clone(),
            creation.row.r#gen,
            creation.row.unit_key.clone(),
            creation.row.unit_content_hash.clone(),
        );
        let Some(unit) = retained_units.get(&identity) else {
            return Ok(());
        };
        // The JSONL record is only an acceleration ledger.  Do not let it
        // mint a durable root unless its exact row is authenticated by a
        // pinned normalized body.
        authenticate_chunk_row(&creation.row, retained_units)?;
        for introduction_commit in &unit.introductions {
            let introduction_config = match introduction_configs.get(introduction_commit) {
                Some(config) => config.clone(),
                None => {
                    let config = match repo.read_commit(introduction_commit) {
                        Ok(commit) => Some(repo.read_tree(&commit.tree)?.chunking_config_hash),
                        Err(error) if is_store_not_found(&error) => None,
                        Err(error) => return Err(error),
                    };
                    introduction_configs.insert(introduction_commit.clone(), config.clone());
                    config
                }
            };
            if introduction_config.as_deref() != Some(creation.row.chunking_config_hash.as_str()) {
                continue;
            }
            let event = ChunkPublicationEvent {
                event: "publication".to_owned(),
                chunk_id: creation.row.chunk_id.clone(),
                chunking_config_hash: creation.row.chunking_config_hash.clone(),
                introduction_commit: introduction_commit.clone(),
            };
            // The introduction tree, not the creation row's timestamp, binds
            // the association to a configuration. This both rejects a later
            // config pasted onto an older commit and preserves legitimate
            // incomparable introductions on separate history branches.
            if authenticate_publication_event_cached(repo, kio_dir, &event, &creation, publication)?
            {
                pending.push(event);
            }
        }
        Ok(())
    })?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssociationAppend {
    Blocked,
    Existing,
    Created,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_new_chunk_association(
    repo: &Repository,
    row: ChunkRow,
    known_associations: &mut BTreeSet<(String, String)>,
    chunk_rowids: &mut BTreeMap<String, u64>,
    next_rowid: &mut u64,
    next_association_rowid: &mut u64,
    appended: &mut Vec<StoredChunk>,
    unit_authorities: &AuthenticatedNormalizedUnits,
) -> Result<AssociationAppend> {
    if purge_blocks_rebuild_raw(repo.kio_dir(), &row.raw_hash, repo.control())? {
        return Ok(AssociationAppend::Blocked);
    }
    authenticate_chunk_row(&row, unit_authorities)?;
    insert_chunk_association(
        repo,
        row,
        known_associations,
        chunk_rowids,
        next_rowid,
        next_association_rowid,
        appended,
    )
}

fn append_authenticated_chunk_association(
    repo: &Repository,
    row: ChunkRow,
    known_associations: &mut BTreeSet<(String, String)>,
    chunk_rowids: &mut BTreeMap<String, u64>,
    next_rowid: &mut u64,
    next_association_rowid: &mut u64,
    appended: &mut Vec<StoredChunk>,
) -> Result<AssociationAppend> {
    if purge_blocks_rebuild_raw(repo.kio_dir(), &row.raw_hash, repo.control())? {
        return Ok(AssociationAppend::Blocked);
    }
    insert_chunk_association(
        repo,
        row,
        known_associations,
        chunk_rowids,
        next_rowid,
        next_association_rowid,
        appended,
    )
}

fn insert_chunk_association(
    repo: &Repository,
    row: ChunkRow,
    known_associations: &mut BTreeSet<(String, String)>,
    chunk_rowids: &mut BTreeMap<String, u64>,
    next_rowid: &mut u64,
    next_association_rowid: &mut u64,
    appended: &mut Vec<StoredChunk>,
) -> Result<AssociationAppend> {
    let association = (row.chunk_id.clone(), row.chunking_config_hash.clone());
    if !known_associations.insert(association) {
        return Ok(AssociationAppend::Existing);
    }
    let rowid = match chunk_rowids.get(&row.chunk_id).copied() {
        Some(rowid) => rowid,
        None => {
            let rowid = *next_rowid;
            *next_rowid = next_rowid
                .checked_add(1)
                .ok_or_else(|| KioError::schema("chunk rowid overflow"))?;
            chunk_rowids.insert(row.chunk_id.clone(), rowid);
            rowid
        }
    };
    persist_chunk_object(repo, &row)?;
    appended.push(StoredChunk {
        rowid,
        association_rowid: *next_association_rowid,
        row,
    });
    *next_association_rowid = next_association_rowid
        .checked_add(1)
        .ok_or_else(|| KioError::schema("chunk/config association rowid overflow"))?;
    Ok(AssociationAppend::Created)
}

/// R16-1/R16-4: read the HEAD commit's tree object for a *write* / rebuild path
/// (index / reindex / repair rebuild-db), folding a missing commit OR tree object
/// (shallow: GC'd / deleted / corrupt CAS) into a clear KIO-E-COMMIT-SHALLOW-001 with
/// recovery guidance instead of a raw, opaque KIO-E-STORE-NOT-FOUND-001. Shared so
/// every rebuild caller gets the same conversion (rationale: the recovery — restore
/// the object or re-create the scope — and semantics are identical to every other
/// shallow-commit site, so no new error code is warranted).
pub(crate) fn read_head_tree_for_rebuild(repo: &Repository, head: &str) -> Result<TreeObject> {
    let commit = match repo.read_commit(head) {
        Ok(commit) => commit,
        Err(error) if is_store_not_found(&error) => return Err(commit_shallow_for_rebuild(head)),
        Err(error) => return Err(error),
    };
    match repo.read_tree(&commit.tree) {
        Ok(tree) => Ok(tree),
        Err(error) if is_store_not_found(&error) => {
            kio_core::gc::GcSweepSession::bind_repository(repo)?
                .validate_final_shallow_tree(head, &commit.tree)?;
            Err(commit_shallow_for_rebuild(head))
        }
        Err(error) => Err(error),
    }
}

fn commit_shallow_for_rebuild(head: &str) -> KioError {
    KioError::commit_shallow(
        "HEAD commit is shallow (commit or tree object discarded); cannot rebuild the \
         index — restore the missing object or re-create the scope",
        head.to_owned(),
    )
}

/// R16-4: which `load_normalized_units` failures are per-document skippable during a
/// rebuild. A missing unit file surfaces as KIO-E-STORE-IO-001 and a malformed
/// manifest/unit as KIO-E-STORE-CORRUPT-001; both are localized to one document's
/// normalized instance, so the rebuild skips that raw_hash and recovers the rest.
pub(crate) fn is_rebuild_skippable_unit_error(error: &KioError) -> bool {
    matches!(
        error.error_code(),
        "KIO-E-STORE-IO-001" | "KIO-E-STORE-CORRUPT-001"
    )
}

/// R16-4: disclose a rebuild's skipped documents on the command output. `skipped_units`
/// is always present (empty on a clean rebuild) so callers can rely on the field; when
/// non-empty it is accompanied by `skipped_units_guidance` pointing at the recovery
/// path. Rationale for keeping the exit code at 0 (rather than a partial-failure exit):
/// the rebuild genuinely succeeded for every recoverable document, the skip is loud in
/// the JSON (never a silent shrink, per R16-4), and there is no store-namespaced
/// partial code to reuse — inventing/borrowing one (a repeat of the R14-5 anti-pattern)
/// would be worse than a self-describing `skipped_units` array.
///
/// R17-6: a skipped normalized-unit does NOT imply the document is unsearchable.
/// `build_sqlite_index_at`/the rebuild re-serve any chunk rows already persisted in
/// `chunks.jsonl`, so a document whose live chunks survive stays fully searchable — only
/// its normalized *source* is stale. Matching each skip's `(raw_hash, gen)` against the
/// persisted chunks separates a stale-but-searchable document (a soft "re-normalize when
/// convenient" note) from one with no live chunks (the genuine "re-normalize now" push),
/// so the emergency guidance — which pointed users at the `reindex --force` R17-2 just
/// un-bricked — no longer false-alarms on a searchable document. Matching on gen too is
/// load-bearing: a reindex may advance a document's gen and then fail to chunk the new
/// gen, leaving only DEAD old-gen chunks that must NOT read as live.
pub(crate) fn skipped_disclosure(report: &Step3RebuildReport, kio_dir: &Path) -> SkippedDisclosure {
    // Best-effort: an unreadable ledger conservatively makes every skip unsearchable.
    let live: BTreeSet<(String, u64)> = read_creation_metadata(kio_dir)
        .unwrap_or_default()
        .into_iter()
        .map(|chunk| (chunk.row.raw_hash, chunk.row.r#gen))
        .collect();
    let mut any_unsearchable = false;
    let entries = report
        .skipped_units
        .iter()
        .map(|unit| {
            let searchable = live.contains(&(unit.raw_hash.clone(), unit.r#gen));
            any_unsearchable |= !searchable;
            PublishedSkippedUnit {
                unit: unit.clone(),
                searchable,
                guidance: if searchable {
                    "re-serving cached chunks; the normalized source is stale — \
                 re-normalize with `kio reindex --force` when convenient"
                } else {
                    "no live chunks remain for this document; run `kio reindex --force` \
                 to re-normalize it from the raw objects"
                }
                .to_owned(),
            }
        })
        .collect::<Vec<_>>();
    let guidance = (!entries.is_empty()).then(|| {
        if any_unsearchable {
            "some documents' normalized units are missing or corrupt and have no live \
         chunks; run `kio reindex --force` to re-normalize them from the raw objects"
        } else {
            "some documents' normalized sources are stale but their cached chunks are \
         still served; re-normalize with `kio reindex --force` when convenient"
        }
        .to_owned()
    });
    SkippedDisclosure {
        skipped_units: entries,
        skipped_units_guidance: guidance,
    }
}

#[derive(Debug, Default)]
pub(crate) struct Step3RebuildReport {
    pub(crate) rebuilt_chunks: u64,
    pub(crate) rebuilt_tree_entries: u64,
    /// R16-4: documents skipped because their normalized units are missing/corrupt,
    /// each `{raw_hash, path, reason}`. Empty on a clean rebuild.
    pub(crate) skipped_units: Vec<SkippedUnit>,
    /// Keep the verified pins and exact exclusions for same-command enrichment.
    pub(crate) enrichment_replay: Option<RebuildAuthorities>,
}

pub(crate) fn read_chunking_config(repo: &Repository) -> Result<ChunkingConfig> {
    let path = repo.kio_dir().join("config.toml");
    let text = fs::read_to_string(&path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
    let max_chars = value
        .get("chunking")
        .and_then(|chunking| chunking.get("max_chars"))
        .and_then(toml::Value::as_integer)
        .filter(|value| *value > 0)
        .map(|value| value as u64)
        .unwrap_or(6000);
    let strategy = value
        .get("chunking")
        .and_then(|chunking| chunking.get("strategy"))
        .and_then(toml::Value::as_str)
        .unwrap_or("heading");
    let hash =
        kio_index::chunking::chunking_config_hash(strategy, max_chars).map_err(index_to_kio)?;
    Ok(ChunkingConfig {
        chunking_config_hash: hash,
        strategy: strategy.to_owned(),
        max_chars,
    })
}

pub(crate) fn store_corrupt_error(path: &Path, message: impl Into<String>) -> KioError {
    let path_string = path.display().to_string();
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        format!("corrupt store file at {path_string}: {}", message.into()),
        json!({ "path": path_string }),
        ExitCode::PermanentFailure,
    )
}

fn rebuild_sqlite_index(
    ctx: &Invocation<'_>,
    repo: &Repository,
    tree_entries: &[TreeEntryRow],
    retained_instances: &[RetainedNormalizedInstance],
    authorities: &mut RebuildAuthorities,
    chunking_config_hash: &str,
) -> Result<()> {
    let adapter_settings = ctx.settings;

    let kio_dir = repo.kio_dir();
    ensure_no_visible_purge_journal(kio_dir, ctx.control)?;
    let path = sqlite_path(kio_dir);
    // O5: a 0-chunk scope (empty folder / secrets-only / text-less PDF) skips
    // `append_stored_chunks` and so never creates `.kio/index/`, but the auto-
    // snapshot still advances HEAD. Create the index dir unconditionally here so
    // opening `sqlite.db` cannot fail with a half-initialized "commit, no index"
    // state that makes every re-index exit 2.
    // Hold the repository-owned `index` directory for the entire build and
    // publication.  The SQLite connection independently binds its open to the
    // same directory identity; this handle is also the authority for cleanup
    // and the final rename, so replacing `.kio/index` with a symlink cannot
    // redirect either operation to another directory.
    let directory = open_chunk_ledger_dir(kio_dir, true)?;
    // P5 (docs/05:564): build the new source index in a unique temp db and atomically
    // rename it over sqlite.db. Writer and local source-cache commands must see a
    // complete database — the old one until the rename, the new one after. Direct
    // search does not open this file; it sees only the writer-published replica.
    // The unique temp name also stops two rebuilders from clobbering one shared
    // `sqlite.db.tmp`.
    let temp_path = unique_sqlite_temp_path(&path);
    let temp_name = temp_path.file_name().ok_or_else(|| {
        KioError::schema(format!(
            "source index temp path has no file name: {}",
            temp_path.display()
        ))
    })?;
    // A residual temp from a crashed rebuild would be reused (and corrupt the new
    // index) by `Connection::open`; start from a clean slate.
    match cap_fs::remove_file(&directory.handle, Path::new(temp_name)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(KioError::io(
                error.to_string(),
                temp_path.display().to_string(),
            ));
        }
    }
    match build_sqlite_index_at(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        &temp_path,
        repo,
        tree_entries,
        retained_instances,
        authorities,
        chunking_config_hash,
    ) {
        Ok(()) => {
            // From the swap onward the old projection no longer describes the
            // source index.  A strict reader cannot validate or repair it, so
            // take the existing header out of service before publishing the
            // replacement and turn it Ready only with the full projection.
            mark_replica_unavailable_or_log(ctx, kio_dir);
            cap_fs::rename(
                &directory.handle,
                Path::new(temp_name),
                &directory.handle,
                Path::new("sqlite.db"),
            )
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
            repo.control()
                .checkpoint(kio_core::durability::DurabilityPoint::ExtractionIndexRenamed)?;
            sync_chunk_ledger_dir(&directory)?;
            // 05 §1.8 write-through, placed on the rename rather than in each
            // command that triggers a rebuild: `index`, `reindex` and
            // `repair rebuild-db` all reach cross-scope search through this
            // one line, and a fourth caller would otherwise have to remember.
            // No rotation here: the rebuild minted a fresh `index_generation`
            // into the temp db moments ago (PB28), so the stamp already moved.
            write_through_projection_or_log_for_repo(ctx, repo);
            Ok(())
        }
        Err(error) => {
            let _ = cap_fs::remove_file(&directory.handle, Path::new(temp_name));
            Err(error)
        }
    }
}

/// Every vector `objects/` holds, matched to the chunks of the index being
/// built — the `objects/` → `embeddings` step of 04 §4.3's rebuild order.
///
/// An object records what the vector is OF (`target_hash` = the chunk's
/// `text_hash`, plus the filename context and the profile) but not which chunk
/// rows carry that text today, because that is exactly the part a rebuild is
/// re-deriving. The join is therefore against the freshly built `chunks` table,
/// and one object legitimately fans out to several chunks — content-based reuse
/// (04 §5.5) is the same identity seen from the other side.
///
/// `context_key` participates in the match: since 07 §5.3's addendum a vector
/// is a function of `(text_hash, context, profile)`, so two chunks with
/// identical bodies in differently named files have different vectors and must
/// not be crossed. Objects whose profile is not the live one are skipped rather
/// than replayed — 04 §4.3 requires the rebuild to bind exactly one candidate
/// per chunk, and several profiles can legitimately coexist in the store.
fn rebuild_image_targets(repo: &Repository) -> Result<Option<BTreeSet<String>>> {
    let Some(expected) = rebuild_embedding_profile_from_tool_lock(repo.kio_dir())? else {
        return Ok(Some(BTreeSet::new()));
    };
    let store = repo.object_store();
    let mut targets = BTreeSet::new();
    for hash in store.embedding_hashes()? {
        let Ok(object) = repo
            .control()
            .checkpoint(kio_core::durability::DurabilityPoint::BeforeImageReplayCandidateRead)
            .and_then(|()| store.read_embedding(&hash))
        else {
            // This is an optimization only. The object may become readable
            // during replay, so keep all verified ownership in this case.
            return Ok(None);
        };
        if object.target_type == "image"
            && object.dimensions == expected.dimensions
            && object.distance == expected.distance
            && object.modality == expected.modality
            && object.profile_hash == expected.profile_hash
        {
            targets.insert(object.target_hash);
        }
    }
    Ok(Some(targets))
}

/// Replay image vectors from `objects/embeddings/` into `embeddings` +
/// `image_vec` (04 §4.3's rebuild order, last step).
///
/// Reads from the CAS rather than from the `embeddings` table because
/// `rebuild-db`'s hard case is a LOST database — the table it would read is the
/// thing being rebuilt. `objects/` is the truth; the tables are the cache.
///
/// Only the current authenticated source and codec identity may be replayed;
/// converted image inputs carry their receipt digest in `context_key`.
fn replay_image_embeddings_from_objects(
    ctx: &Invocation<'_>,
    repo: &Repository,
    conn: &Connection,
    expected_profile: Option<&embedding_store::EmbeddingProfileSummary>,
    sources: (&[RetainedNormalizedInstance], &RebuildAuthorities),
    retained_chunks: &[RetainedEmbeddingChunk],
    secrets_allowed: bool,
) -> Result<()> {
    let (retained_instances, authorities) = sources;
    let Some(expected) = expected_profile else {
        return Ok(());
    };
    if authorities.image_units.is_none() {
        return Ok(());
    }
    let store = repo.object_store();
    let hashes = store.embedding_hashes()?;
    let policy = current_embedding_policy(repo)?;
    let authority = authorities.replay_image_authority(retained_instances, &policy)?;
    let scope_id = repo.scope_identity()?.scope_id;
    let eligible = crate::embedding::input::referenced_authorized_image_hashes_for_chunks(
        retained_chunks,
        &scope_id,
        &authority,
        secrets_allowed,
    );
    for hash in hashes {
        let object = match store.read_embedding(&hash) {
            Ok(object) => object,
            Err(error) => {
                // Same posture as the chunk replay: a cache layer may not
                // refuse to rebuild over one bad file.
                ctx.interaction.diagnostic(&format!(
                    "kio: skipping unreadable embedding object {hash}: {error}"
                ));
                continue;
            }
        };
        if object.target_type != "image" || !eligible.contains(&object.target_hash) {
            continue;
        }
        if object.dimensions != expected.dimensions
            || object.distance != expected.distance
            || object.modality != expected.modality
            || object.profile_hash != expected.profile_hash
        {
            continue;
        }
        // Re-derive the current codec identity from the authenticated source.
        // An immutable object produced by a retired codec may remain in CAS,
        // but must never become the current vector during a lost-DB rebuild.
        let current = (|| -> Result<_> {
            let (bytes, mime) = crate::embedding::input::read_verified_embedding_image(
                &store,
                &object.target_hash,
            )?;
            crate::embedding::image_input::prepare_embedding_image(&object.target_hash, bytes, mime)
        })();
        let prepared = match current {
            Ok(prepared) => prepared,
            Err(error) => {
                ctx.interaction.diagnostic(&format!(
                    "kio: skipping image embedding {hash} without verified source: {error}"
                ));
                continue;
            }
        };
        let input_hash = prepared
            .receipt
            .as_ref()
            .map(|receipt| receipt.image_input_hash())
            .transpose()?;
        let context =
            input_hash
                .as_ref()
                .map(|image_input_hash| kio_core::cas::EmbeddingContext::Image {
                    image_input_hash: image_input_hash.clone(),
                });
        if object.context != context || object.metadata.image_input_receipt != prepared.receipt {
            continue;
        }
        embedding_store::write_image_embedding(
            conn,
            &hash,
            &object.target_hash,
            &embedding_store::f32_to_le_bytes(&object.vector),
            object.dimensions,
            &object.distance,
            &object.modality,
            &object.profile_hash,
            input_hash.as_deref(),
        )
        .map_err(index_to_kio)?;
    }
    Ok(())
}

/// A chunk-vector replay row derived solely from an embedding CAS object.
/// It is local to rebuild; the old public snapshot type represented SQLite as
/// an input source and was deliberately removed with that compatibility path.
struct ReplayedChunkEmbedding {
    embedding_hash: String,
    text_hash: String,
    chunk_id: String,
    vector: Vec<u8>,
    dimensions: u64,
    distance: String,
    modality: String,
    profile_hash: String,
    context_key: Option<String>,
}

fn embeddings_from_objects(
    ctx: &Invocation<'_>,
    repo: &Repository,
    expected_profile: Option<&embedding_store::EmbeddingProfileSummary>,
    retained_chunks: &[RetainedEmbeddingChunk],
) -> Result<Vec<ReplayedChunkEmbedding>> {
    let store = repo.object_store();
    let hashes = store.embedding_hashes()?;
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    // The persisted current tool-lock selects the one vector space this rebuilt
    // projection may contain. It deliberately does not consult adapter
    // activation: an offline repair must restore the space recorded at index
    // time, while a lock without `embedding` is an explicitly text-only scope.
    let mut chunks_by_text: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    for chunk in retained_chunks {
        chunks_by_text
            .entry(chunk.text_hash.clone())
            .or_default()
            .push((
                chunk.chunk_id.clone(),
                embedding_store::chunk_embedding_context(&chunk.raw_path),
            ));
    }

    let mut replayed = Vec::new();
    for hash in hashes {
        let object = match store.read_embedding(&hash) {
            Ok(object) => object,
            Err(error) => {
                // A cache layer may not refuse to rebuild over one bad file.
                // `repair verify-objects` is what reports and quarantines it.
                ctx.interaction.diagnostic(&format!(
                    "kio: skipping unreadable embedding object {hash}: {error}"
                ));
                continue;
            }
        };
        if object.target_type != "chunk" {
            continue;
        }
        let Some(expected) = expected_profile else {
            continue;
        };
        if object.dimensions != expected.dimensions
            || object.distance != expected.distance
            || object.modality != expected.modality
            || object.profile_hash != expected.profile_hash
        {
            continue;
        }
        let Some(candidates) = chunks_by_text.get(&object.target_hash) else {
            continue;
        };
        let vector = kio_index::embedding_store::f32_to_le_bytes(&object.vector);
        for (chunk_id, context) in candidates {
            if context.as_deref()
                != object
                    .context
                    .as_ref()
                    .and_then(kio_core::cas::EmbeddingContext::as_text)
            {
                continue;
            }
            replayed.push(ReplayedChunkEmbedding {
                embedding_hash: hash.clone(),
                text_hash: object.target_hash.clone(),
                chunk_id: chunk_id.clone(),
                vector: vector.clone(),
                dimensions: object.dimensions,
                distance: object.distance.clone(),
                modality: object.modality.clone(),
                profile_hash: object.profile_hash.clone(),
                context_key: object
                    .context
                    .as_ref()
                    .and_then(kio_core::cas::EmbeddingContext::as_text)
                    .map(str::to_owned),
            });
        }
    }
    Ok(replayed)
}

/// A unique sibling temp path for the atomic sqlite rebuild
/// (`sqlite.db.<pid>-<nanos>.tmp`, P5). Unique per attempt so concurrent
/// rebuilders never share a temp.
fn unique_sqlite_temp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let file_name = format!("sqlite.db.{}-{nanos}.tmp", process::id());
    match path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    }
}

/// A tagged introduction is authoritative only when its claimed commit pins the
/// exact immutable normalized unit from which the creation chunk was derived.
/// A missing commit is the normal pre-commit crash residue; an existing but
/// unrelated commit is durable-ledger corruption and must fail closed.
#[allow(dead_code)] // Kept as the one-shot crate API for non-batch callers.
pub(crate) fn authenticate_publication_event(
    repo: &Repository,
    kio_dir: &Path,
    event: &ChunkPublicationEvent,
    creation: &StoredChunk,
) -> Result<bool> {
    authenticate_publication_event_cached(
        repo,
        kio_dir,
        event,
        creation,
        &mut PublicationAuthenticationCache::default(),
    )
}

/// Operation-local cache for authenticating append-only publication ledgers.
/// The cache contains only immutable CAS reads, and its keys retain the exact
/// creation unit tuple so a successful attestation is never reused for a
/// different row.
#[derive(Debug, Default)]
pub(crate) struct PublicationAuthenticationCache {
    commits: BTreeMap<String, Option<CommitObject>>,
    trees: BTreeMap<String, CachedPublicationTree>,
    pinned_unit_closures: BTreeMap<NormalizedInstanceCacheKey, BTreeSet<(String, String)>>,
    unavailable_pins: BTreeSet<NormalizedInstanceCacheKey>,
    unreadable_unit_keys: BTreeMap<NormalizedInstanceCacheKey, BTreeSet<String>>,
    attestations: BTreeMap<PublicationAttestationCacheKey, bool>,
}

#[derive(Debug)]
struct CachedPublicationTree {
    chunking_config_hash: String,
    normalized_entries: BTreeMap<TreeNormalizedEntryKey, BTreeSet<NormalizeRef>>,
}

type TreeNormalizedEntryKey = (String, String, u64);
type NormalizedInstanceCacheKey = (String, String, u64, String);
type PublicationAttestationCacheKey = (String, String, String, String, u64, String, String);

pub(crate) fn authenticate_publication_event_cached(
    repo: &Repository,
    kio_dir: &Path,
    event: &ChunkPublicationEvent,
    creation: &StoredChunk,
    cache: &mut PublicationAuthenticationCache,
) -> Result<bool> {
    if event.chunk_id != creation.row.chunk_id
        || event.chunking_config_hash != creation.row.chunking_config_hash
    {
        return Err(store_corrupt_error(
            &chunks_jsonl_path(kio_dir),
            "publication event does not match its creation chunk/config association",
        ));
    }
    let attestation_key = (
        event.introduction_commit.clone(),
        event.chunking_config_hash.clone(),
        creation.row.raw_hash.clone(),
        creation.row.tool_profile_hash.clone(),
        creation.row.r#gen,
        creation.row.unit_key.clone(),
        creation.row.unit_content_hash.clone(),
    );
    if let Some(attested) = cache.attestations.get(&attestation_key) {
        return Ok(*attested);
    }

    let commit = match cache.commits.get(&event.introduction_commit) {
        Some(Some(commit)) => commit.clone(),
        Some(None) => return Ok(false),
        None => match repo.read_commit(&event.introduction_commit) {
            Ok(commit) => {
                cache
                    .commits
                    .insert(event.introduction_commit.clone(), Some(commit.clone()));
                commit
            }
            Err(error) if is_store_not_found(&error) => {
                cache
                    .commits
                    .insert(event.introduction_commit.clone(), None);
                return Ok(false);
            }
            Err(error) => return Err(error),
        },
    };
    if !cache.trees.contains_key(&commit.tree) {
        let tree = repo.read_tree(&commit.tree)?;
        let mut normalized_entries =
            BTreeMap::<TreeNormalizedEntryKey, BTreeSet<NormalizeRef>>::new();
        for entry in tree.entries {
            let entry = entry?;
            let Some(normalize) = entry.normalize else {
                continue;
            };
            normalized_entries
                .entry((
                    entry.raw_hash,
                    normalize.tool_profile_hash.clone(),
                    normalize.r#gen,
                ))
                .or_default()
                .insert(normalize);
        }
        cache.trees.insert(
            commit.tree.clone(),
            CachedPublicationTree {
                chunking_config_hash: tree.chunking_config_hash,
                normalized_entries,
            },
        );
    }
    let cached_tree = cache
        .trees
        .get(&commit.tree)
        .expect("cached tree index was just inserted");
    if cached_tree.chunking_config_hash != event.chunking_config_hash {
        return Err(store_corrupt_error(
            &chunks_jsonl_path(kio_dir),
            "publication event chunking config is not bound by its introduction commit tree",
        ));
    }
    let normalizes = cached_tree
        .normalized_entries
        .get(&(
            creation.row.raw_hash.clone(),
            creation.row.tool_profile_hash.clone(),
            creation.row.r#gen,
        ))
        .cloned()
        .unwrap_or_default();
    let mut skipped_attestation = false;
    for normalize in normalizes {
        let unit_key = (
            creation.row.raw_hash.clone(),
            normalize.tool_profile_hash.clone(),
            normalize.r#gen,
            normalize.manifest_hash.clone(),
        );
        if let Some(keys) = cache.unreadable_unit_keys.get(&unit_key) {
            // A structurally valid pin can identify the skipped unit but
            // cannot attest a chunk without a verified unit closure.
            skipped_attestation |= keys.contains(&creation.row.unit_key);
            continue;
        }
        if !cache.pinned_unit_closures.contains_key(&unit_key) {
            let manifest = pinned_manifest(repo, &creation.row.raw_hash, &normalize)?;
            let mut closure = BTreeSet::new();
            visit_validated_normalized_units(&repo.object_store(), &manifest, |unit| {
                closure.insert((
                    unit.unit_key,
                    kio_core::cas::hash_bytes(unit.markdown.as_bytes()),
                ));
                Ok(())
            })
            .map_err(pipeline_to_kio)?;
            cache.pinned_unit_closures.insert(unit_key.clone(), closure);
        }
        if cache
            .pinned_unit_closures
            .get(&unit_key)
            .expect("cached pinned unit closure was just inserted")
            .contains(&(
                creation.row.unit_key.clone(),
                creation.row.unit_content_hash.clone(),
            ))
        {
            if cache.unavailable_pins.contains(&unit_key) {
                skipped_attestation = true;
                continue;
            }
            cache.attestations.insert(attestation_key, true);
            return Ok(true);
        }
    }
    if skipped_attestation {
        return Ok(false);
    }
    Err(store_corrupt_error(
        &chunks_jsonl_path(kio_dir),
        "publication event introduction commit does not attest its chunk's pinned normalized unit",
    ))
}

/// Populate a fresh sqlite index at `temp_path` (chunks + FTS, tree_entries,
/// preserved embeddings, chunk_vec) and close it. The caller renames it over
/// sqlite.db (P5); the connection is dropped here so the rename sees no open
/// handle / leftover journal.
fn build_sqlite_index_at(
    invocation: Invocation<'_>,
    temp_path: &Path,
    repository: &Repository,
    tree_entries: &[TreeEntryRow],
    retained_instances: &[RetainedNormalizedInstance],
    authorities: &mut RebuildAuthorities,
    _chunking_config_hash: &str,
) -> Result<()> {
    let RebuildAuthorities {
        units: retained_unit_introductions,
        publication,
        ..
    } = authorities;
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    let kio_dir = repository.kio_dir();
    ensure_no_visible_purge_journal(kio_dir, repository.control())?;
    let mut fts = SqliteFtsIndex::open(
        temp_path,
        FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    // R11-4 (sibling of R10-8's `insert_snapshot_tree_entries`): wrap the whole
    // rebuild — every stored chunk, every tree_entries row, and every preserved
    // embedding — in ONE transaction. Without it each INSERT autocommits with its
    // own fsync, so even a no-op reindex re-pays the full-corpus write cost, which
    // grows without bound because `chunks.jsonl` is append-only (docs/04:334, the
    // time-travel substrate). A manual BEGIN/COMMIT is used rather than an
    // `unchecked_transaction()` guard because `index_chunk` takes `&mut self`; a
    // held guard would keep `fts` immutably borrowed for its lifetime (E0502).
    // Crash safety is unchanged: the caller builds into a unique temp db and
    // removes it on ANY Err (P5, line ~2779), and dropping `fts` rolls back an
    // uncommitted transaction, so a partial/rolled-back temp is never renamed in.
    fts.connection()
        .execute_batch("BEGIN")
        .map_err(|err| KioError::schema(err.to_string()))?;
    let mut live_chunk_ids = BTreeSet::new();
    let mut live_associations = BTreeSet::new();
    let durable_publications = read_chunk_publication_events(kio_dir)?;
    let mut stored_chunks_by_association = BTreeMap::new();
    visit_stored_chunks(kio_dir, |mut chunk| {
        if purge_blocks_rebuild_raw(kio_dir, &chunk.row.raw_hash, repository.control())? {
            return Ok(());
        }
        let identity = (
            chunk.row.raw_hash.clone(),
            chunk.row.tool_profile_hash.clone(),
            chunk.row.r#gen,
            chunk.row.unit_key.clone(),
            chunk.row.unit_content_hash.clone(),
        );
        let Some(_unit) = retained_unit_introductions.get(&identity) else {
            // A durable chunk ledger is not a historical authority. This unit
            // is absent or non-Done in every manifest pinned by a current ref,
            // so replaying its old row would back-publish stale content.
            return Ok(());
        };
        authenticate_chunk_row(&chunk.row, retained_unit_introductions)?;
        live_chunk_ids.insert(chunk.row.chunk_id.clone());
        live_associations.insert((
            chunk.row.chunk_id.clone(),
            chunk.row.chunking_config_hash.clone(),
        ));
        persist_chunk_object(repository, &chunk.row)?;
        // A creation row only contributes its immutable chunk and config
        // association. Publication authority is replayed exclusively from a
        // matching authenticated tagged event below.
        fts.index_chunk_with_rowids(&chunk.row, Some(chunk.rowid), Some(chunk.association_rowid))
            .map_err(index_to_kio)?;
        chunk.row.text = String::new();
        stored_chunks_by_association.insert(
            (
                chunk.row.chunk_id.clone(),
                chunk.row.chunking_config_hash.clone(),
            ),
            chunk,
        );
        Ok(())
    })?;
    // Replay event triples only after all creation associations received their
    // durable explicit rowids.  Event rows intentionally use SQLite's auto
    // rowid; doing this inside the loop could occupy a future explicit rowid.
    for event in &durable_publications {
        if !live_associations
            .contains(&(event.chunk_id.clone(), event.chunking_config_hash.clone()))
        {
            continue;
        }
        let creation = stored_chunks_by_association
            .get(&(event.chunk_id.clone(), event.chunking_config_hash.clone()))
            .expect("validated publication event has its creation association");
        if !authenticate_publication_event_cached(
            repository,
            kio_dir,
            event,
            creation,
            publication,
        )? {
            continue;
        }
        // Creation/order metadata was already replayed with the durable
        // association row. Only this tagged relation carries a historical
        // introduction, including disconnected branch introductions.
        kio_index::fts::record_chunk_publication(
            fts.connection(),
            &event.chunk_id,
            &event.chunking_config_hash,
            &event.introduction_commit,
        )
        .map_err(index_to_kio)?;
    }
    for entry in tree_entries {
        if purge_blocks_rebuild_raw(kio_dir, &entry.raw_hash, repository.control())? {
            continue;
        }
        fts.connection()
            .execute(
                "INSERT OR REPLACE INTO tree_entries(commit_hash, path, raw_hash, tool_profile_hash, gen, manifest_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    entry.commit_hash,
                    entry.path,
                    entry.raw_hash,
                    entry.tool_profile_hash,
                    entry.r#gen,
                    entry.manifest_hash
                ],
            )
            .map_err(|err| KioError::schema(err.to_string()))?;
    }
    // Replay embeddings (source of truth) and re-derive chunk_vec.
    //
    // `objects/` are the sole vector source.  An old SQLite row may be stale,
    // malformed, or a remnant of a previous contract and must never be carried
    // through a rebuild.
    let locked_embedding = rebuild_embedding_entry_from_tool_lock(kio_dir)?;
    let expected_embedding_profile = locked_embedding.as_ref().map(embedding_profile_summary);
    let secrets_allowed = match locked_embedding.as_ref() {
        Some(entry) if persisted_embedding_is_deterministic(entry) => true,
        Some(entry) => approvals::allowed(
            adapter_settings,
            repository,
            approvals::AdapterRole::Embedding,
            &entry.tool_id,
            &entry.profile_hash,
            grants::GrantOperation::SendSecrets,
        )?,
        None => false,
    };
    let retained_embedding_chunks = if expected_embedding_profile.is_some() {
        embedding_owners::collect_retained_embedding_chunks_with_replay(
            repository,
            fts.connection(),
            retained_instances,
            None,
            None,
            Some(authorities),
        )?
    } else {
        Vec::new()
    };
    let embedding_policy = current_embedding_policy(repository)?;
    let selected_contexts =
        selected_embedding_contexts(&retained_embedding_chunks, &embedding_policy)?;
    for row in embeddings_from_objects(
        ctx,
        repository,
        expected_embedding_profile.as_ref(),
        &retained_embedding_chunks,
    )? {
        if !live_chunk_ids.contains(&row.chunk_id) {
            continue;
        }
        embedding_store::write_chunk_embedding_source(
            fts.connection(),
            &row.embedding_hash,
            &row.text_hash,
            &row.vector,
            row.dimensions,
            &row.distance,
            &row.modality,
            &row.profile_hash,
            row.context_key.as_deref(),
        )
        .map_err(index_to_kio)?;
    }
    // R20-10: exclude secret-held chunks so the content-hash rebuild can't link a held
    // (Tier B) chunk to a non-secret content-twin's vector and expose it in vector search.
    let mut held_chunk_ids = BTreeSet::new();
    if !secrets_allowed {
        held_chunk_ids.extend(held_secret_embedding_chunk_ids(
            kio_dir,
            repository.control(),
        )?);
        held_chunk_ids.extend(
            retained_embedding_chunks
                .iter()
                .filter(|chunk| chunk.requires_secret_approval)
                .map(|chunk| chunk.chunk_id.clone()),
        );
    }
    embedding_policy.revalidate().map_err(pipeline_to_kio)?;
    embedding_store::rebuild_chunk_vec_for_contexts(
        fts.connection(),
        expected_embedding_profile.as_ref(),
        &held_chunk_ids,
        &selected_contexts,
    )
    .map_err(index_to_kio)?;
    // 04 §4.3's rebuild order ends `… → chunk_vec → image_vec`.
    replay_image_embeddings_from_objects(
        ctx,
        repository,
        fts.connection(),
        expected_embedding_profile.as_ref(),
        (retained_instances, authorities),
        &retained_embedding_chunks,
        secrets_allowed,
    )?;
    embedding_store::rebuild_image_vec(fts.connection(), expected_embedding_profile.as_ref())
        .map_err(index_to_kio)?;
    // PB28 (step4b-contract-tests-p2b.md §J, §Z ruling 4; 04-pipeline.md §5.7
    // L913 / 05-runtime.md §3.5 L760-761): mint a fresh `index_generation` and
    // initialize `last_lifecycle_epoch` to the CURRENT counter value, in the
    // SAME transaction as the rest of this rebuild — not a separate step
    // afterward (the old `recover_index_generation` call after this function
    // returns left a crash window in which the new temp db could be renamed
    // in with a stale/absent generation). This build targets a brand-new temp
    // db (`SqliteFtsIndex::open` created its `index_metadata` table moments
    // ago, always empty), so `ensure_index_metadata`'s "insert only if
    // absent" is exactly the unconditional first write wanted here.
    // `recover_index_generation`'s later call (still invoked by every
    // `run_repair`/`run_index`/`run_reindex` caller) becomes a no-op in the
    // common case and remains as defense-in-depth self-heal for a crash
    // between this COMMIT and that later call.
    kio_index::fts::ensure_index_metadata(
        fts.connection(),
        &new_ulid(kio_dir),
        PurgeState::open_with_control(kio_dir, repository.control().clone())?
            .read_lifecycle_epoch()?,
    )
    .map_err(index_to_kio)?;
    fts.connection()
        .execute_batch("COMMIT")
        .map_err(|err| KioError::schema(err.to_string()))?;
    drop(fts);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kio_core::cas::{ContentObjectKind, hash_bytes};
    use kio_pipeline::markdownize::{
        MarkdownizeMode, NormalizedUnitManifestEntry, NormalizedUnitObject, PreparationProvenance,
    };
    use kio_pipeline::prepare::{UnitType, unit_ref};

    #[test]
    fn p0_sixth_empty_rebuild_does_not_need_record_stages() {
        let root = tempfile::tempdir().unwrap();
        let control = ControlHandle::default();
        let crate::management::ExplicitRoot::Created(repo) =
            crate::management::initialize_explicit_root(root.path(), &control).unwrap()
        else {
            panic!("fresh root must be created");
        };
        kio_core::store_dir::StoreDirectory::open(repo.kio_dir())
            .unwrap()
            .create_directory_all(Path::new("internal"))
            .unwrap();
        let stages = repo.kio_dir().join("internal/record-stages");
        if stages.is_dir() {
            fs::rename(&stages, stages.with_extension("saved")).unwrap();
        }
        fs::write(&stages, b"record stages unavailable").unwrap();
        let report = rebuild_step3_index(
            &Invocation {
                context: &crate::context::test_support::context(root.path()),
                settings: &Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default()),
                control: &control,
                locks: &kio_core::scope::LockSession::default(),
            },
            &repo,
        )
        .unwrap();
        assert_eq!(report.rebuilt_chunks, 0);
        assert_eq!(report.rebuilt_tree_entries, 0);
    }

    #[test]
    fn p0_fifth_image_ownership_is_only_staged_for_image_replay() {
        let root = tempfile::tempdir().unwrap();
        let crate::management::ExplicitRoot::Created(repo) =
            crate::management::initialize_explicit_root(root.path(), &ControlHandle::default())
                .unwrap()
        else {
            panic!("fresh root must be created");
        };
        let store = repo.object_store();
        let raw_hash = hash_bytes(b"source");
        let profile = hash_bytes(b"profile");
        let image_hash = hash_bytes(b"owned image");
        let generated_at = "2026-11-20T00:00:00Z".to_owned();
        let mut manifest = NormalizedInstanceManifest {
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "p0-image-ownership".into(),
            preparation_provenance: PreparationProvenance::local(),
            units: Vec::new(),
            generated_at: generated_at.clone(),
        };
        for order in 0..2 {
            let unit = NormalizedUnitObject {
                unit_key: format!("page:{}", order + 1),
                unit_type: UnitType::Page,
                raw_hash: raw_hash.clone(),
                prepared_hash: hash_bytes(b"prepared"),
                preparation_profile_hash: hash_bytes(b"preparation profile"),
                tool_profile_hash: profile.clone(),
                r#gen: 0,
                mode: MarkdownizeMode::Full,
                markdown: format!("page {order}"),
                owned_image_hashes: BTreeSet::from([image_hash.clone()]),
                metadata: BTreeMap::new(),
                reused_from: None,
                generated_at: generated_at.clone(),
            };
            let hash = store
                .write_content_object(
                    ContentObjectKind::NormalizedUnit,
                    &serde_jcs::to_vec(&unit).unwrap(),
                )
                .unwrap();
            manifest.units.push(NormalizedUnitManifestEntry {
                order,
                unit_ref: unit_ref(&unit.unit_key),
                unit_key: unit.unit_key,
                unit_type: unit.unit_type,
                status: UnitStatus::Done,
                prepared_hash: unit.prepared_hash,
                preparation_profile_hash: unit.preparation_profile_hash,
                unit_object_hash: Some(hash),
                resource_limit: None,
                error_kind: None,
            });
        }
        let manifest_hash = store
            .write_content_object(
                ContentObjectKind::Manifest,
                &serde_jcs::to_vec(&manifest).unwrap(),
            )
            .unwrap();
        let instances = [RetainedNormalizedInstance {
            raw_hash,
            normalize: NormalizeRef {
                tool_profile_hash: profile,
                r#gen: 0,
                manifest_hash,
            },
            raw_path: "public.pdf".to_owned(),
            policy_paths: BTreeSet::from(["public.pdf".to_owned()]),
            introductions: Vec::new(),
        }];
        let targets = rebuild_image_targets(&repo).unwrap();
        assert_eq!(targets, Some(BTreeSet::new()));
        let (authorities, _) =
            stage_rebuild_authorities(&repo, &instances, BTreeSet::new(), targets.as_ref())
                .unwrap();
        assert!(authorities.image_units.is_none());

        let (authorities, _) = stage_rebuild_authorities(
            &repo,
            &instances,
            BTreeSet::new(),
            Some(&BTreeSet::from([image_hash.clone()])),
        )
        .unwrap();
        let stage = authorities.image_units.as_ref().unwrap();
        assert_eq!(
            stage.len(),
            2,
            "each unit contributes an owned-image record"
        );
        let policy = current_embedding_policy(&repo).unwrap();
        let authority = authorities
            .replay_image_authority(&instances, &policy)
            .unwrap();
        assert!(authority.permits(&image_hash));
    }
}
