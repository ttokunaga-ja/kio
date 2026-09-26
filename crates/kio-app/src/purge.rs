//! Full-history purge orchestration.
//!
//! Target discovery is deliberately CAS-backed: path mode walks every parent
//! commit/tree, while raw-hash mode remains usable when unrelated history is
//! shallow.  Mutable manifests and index/cache projections are never accepted as
//! purge truth.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::commands::PurgeArgs;
use kio_core::cas::{
    ChunkObject, ContentObjectKind, ObjectKind, ObjectStore, canonical_json_bytes, hash_bytes,
    is_hash,
};
use kio_core::dag::CommitType;
use kio_core::dag::TreeEntry;
use kio_core::history::HistoryReader;
use kio_core::purge::{
    BeginOutcome, ClosureItem, EventKind, LifecycleEvent, PurgeClosure, PurgeJournal, PurgePhase,
    PurgeReason, PurgeState, TombstoneMode, canonical_final_event, closure_content_hash,
};
use kio_core::scope::{
    Repository, StoreLock, append_event_log, cleanup_orphan_raw_ingest_temps, now_utc_seconds,
};
#[cfg(debug_assertions)]
use kio_core::test_control::PurgeFault;
use kio_core::{ExitCode, KioError, Result};
use kio_index::fts::{
    ExistingSourceIndexOpenMode, FtsSchemaConfig, FtsTokenizer, SqliteFtsIndex,
    open_existing_source_index_connection,
};
use kio_pipeline::ledger::TaskKey;
use kio_pipeline::ledger::ops::{
    AbandonResolution, AbandonSelector, get_batch_request, recovery_finish_cleanup,
    recovery_settle_unknown, resolve_abandon_selector,
};
use kio_pipeline::markdownize::{
    NormalizedInstanceManifest, NormalizedUnitObject, UnitStatus,
    load_validated_normalized_units_from_manifest,
};
use kio_pipeline::task::{MAX_TASK_STORE_BYTES, TaskStore, TaskType};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const VALIDATION_HASH: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Eq)]
struct PurgePlan {
    /// HEAD observed while deriving the target. It is rechecked under the store
    /// lock before the journal can be created.
    head_commit: String,
    target_raw_hashes: Vec<String>,
    /// Known historical names are audit/log-scrub inputs only, never target
    /// authority. Raw-hash mode intentionally does not require a history walk.
    historical_paths: Vec<String>,
    reason: PurgeReason,
    tombstone_mode: TombstoneMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletedTerminal {
    purged_in_commit: String,
    tombstone_count: u64,
    erase_receipt_count: u64,
}

#[derive(Debug)]
struct PurgePreview {
    plan: PurgePlan,
    completed: Option<CompletedTerminal>,
    /// PA37 (§K, U34; §R ruling #1): count of working-tree entries whose
    /// bytes match a purge target — informational only (ruling #1 retired
    /// the prior hard block).
    working_tree_alias_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeletedCounts {
    raw_objects: u64,
    chunk_objects: u64,
    chunk_ledger_rows: u64,
    sqlite_chunks: u64,
    sqlite_associations: u64,
    sqlite_vectors: u64,
    sqlite_orphan_embeddings: u64,
    normalized_instances: u64,
    normalized_views: u64,
    prepared_objects: u64,
    image_objects: u64,
    tasks: u64,
    reservations: u64,
    manifest_rows: u64,
    unsupported_rows: u64,
    quarantine_rows: u64,
    cache_directories: u64,
    #[serde(default)]
    staging_descriptors: u64,
    /// PB04/item 3: `objects/manifests/` CAS objects removed for this
    /// purge's target instances. Distinct from `manifest_rows` (the
    /// scope-level `.kio/manifest.json` file-inventory row count).
    #[serde(default)]
    manifest_objects: u64,
    /// Immutable normalized-unit CAS bodies selected by target manifest
    /// objects. These are distinct from the mutable normalized instance cache.
    #[serde(default)]
    normalized_unit_objects: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SharedArtifactsPreserved {
    prepared_objects: u64,
    image_objects: u64,
    #[serde(default)]
    normalized_unit_objects: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PurgeReport {
    purged_in_commit: Option<String>,
    deleted: DeletedCounts,
    shared: SharedArtifactsPreserved,
    tombstone_count: u64,
    erase_receipt_count: u64,
    log_files_scrubbed: u64,
    log_rows_removed: u64,
    log_fields_masked: u64,
}

pub(crate) fn run(args: PurgeArgs) -> Result<Value> {
    let repo = crate::open_current_recovery_repository()?;
    // Orphan cleanup is transaction recovery, not target mutation. Run it under
    // the store lock before preview/working-copy refusal so a tombstoned raw temp
    // is removed even when the requested purge cannot proceed.
    {
        let _store_lock = repo.lock_store()?;
        repo.ensure_no_pending_managed_restore()?;
        let state = PurgeState::open(repo.kio_dir())?;
        if state.read_journal()?.is_some() {
            // A legacy closure cannot prove a complete embedding target set.
            // Reject it before even orphan cleanup can remove target bytes.
            state.read_closure()?;
        }
        cleanup_orphan_raw_ingest_temps(repo.kio_dir())?;
    }
    let preview = preflight(&repo, &args)?;
    confirm(&preview, args.yes)?;
    execute_phase_machine(&repo, &args, &preview)
}

/// The actor recorded on new journal/lifecycle records (05-runtime.md §3.5's
/// `actor` field). Matches the existing `KIO-PURGE-COMPLETED` log convention.
fn current_actor() -> String {
    std::env::var("USER").unwrap_or_else(|_| "local-user".to_owned())
}

/// PA43-46 (§N, §R ruling #2): compute the full purge closure once, at
/// `prepared` — the same shared-vs-removable classification
/// `delete_derived_surfaces` used to recompute live on every entry (fresh
/// *and* resumed) is now computed exactly once, here, and durably fixed via
/// the `.kio/purge/journal-closure` sidecar before the journal referencing it
/// (by content hash) is created. `chunk` items are the raw-hash-membership
/// target set (deterministic — this purge's own targets only, no cross-purge
/// drift risk, unlike `prepared`/`image` sharing with OTHER raws). Embedding
/// identities and shared-text/image preservation are also fixed here, before
/// SQLite can erase the rows that made a vector discoverable. SQLite metadata
/// must agree with the ledger; only immutable history and semantic CAS grant
/// target or survivor authority.
fn compute_purge_closure(
    repo: &Repository,
    plan: &PurgePlan,
    purge_id: &str,
) -> Result<PurgeClosure> {
    let targets = plan
        .target_raw_hashes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut items = plan
        .target_raw_hashes
        .iter()
        .map(|raw_hash| ClosureItem {
            object_type: "raw".to_owned(),
            hash: raw_hash.clone(),
        })
        .collect::<Vec<_>>();

    let stored_chunks = crate::read_stored_chunks(repo.kio_dir())?;
    let embeddings = purge_embedding_inventory(repo)?;
    let surviving_text =
        authenticate_purge_chunk_inventory(repo, &targets, &stored_chunks, &embeddings)?;
    for stored in &stored_chunks {
        if targets.contains(stored.row.raw_hash.as_str()) {
            items.push(ClosureItem {
                object_type: "chunk".to_owned(),
                hash: stored.row.chunk_id.clone(),
            });
        }
    }

    let store = repo.object_store();
    let mut inventory = scan_derived_inventory(&store, repo.kio_dir(), &targets)?;
    extend_inventory_from_reachable_refs(repo, &targets, &mut inventory)?;
    let shared_prepared = inventory
        .target_prepared
        .intersection(&inventory.surviving_prepared)
        .cloned()
        .collect::<BTreeSet<_>>();
    let shared_images = inventory
        .target_images
        .intersection(&inventory.surviving_images)
        .cloned()
        .collect::<BTreeSet<_>>();
    for hash in inventory.target_prepared.difference(&shared_prepared) {
        items.push(ClosureItem {
            object_type: "prepared".to_owned(),
            hash: hash.clone(),
        });
    }
    for hash in inventory.target_images.difference(&shared_images) {
        items.push(ClosureItem {
            object_type: "image".to_owned(),
            hash: hash.clone(),
        });
    }
    // PB04/item 3: manifest objects are never shared across raws (unlike
    // prepared/image, keyed by their own instance's exact JCS bytes), so
    // every target manifest hash is unconditionally removable — no
    // surviving/preserved split to compute.
    for hash in &inventory.target_manifest_hashes {
        items.push(ClosureItem {
            object_type: "manifest".to_owned(),
            hash: hash.clone(),
        });
    }
    // A normalized-unit CAS body is normally impossible to share across raw
    // identities: its canonical body embeds `raw_hash`. Still, do not rely on
    // that invariant as the deletion authority. A surviving manifest pin wins
    // conservatively, and leaves the body intact if malformed state somehow
    // creates a shared reference.
    let shared_normalized_units = inventory
        .target_normalized_units
        .intersection(&inventory.surviving_normalized_units)
        .cloned()
        .collect::<BTreeSet<_>>();
    for hash in inventory
        .target_normalized_units
        .difference(&shared_normalized_units)
    {
        items.push(ClosureItem {
            object_type: "normalized_unit".to_owned(),
            hash: hash.clone(),
        });
    }

    let mut preserved = Vec::new();
    for hash in &shared_prepared {
        preserved.push(ClosureItem {
            object_type: "prepared".to_owned(),
            hash: hash.clone(),
        });
    }
    for hash in &shared_images {
        preserved.push(ClosureItem {
            object_type: "image".to_owned(),
            hash: hash.clone(),
        });
    }
    for hash in &shared_normalized_units {
        preserved.push(ClosureItem {
            object_type: "normalized_unit".to_owned(),
            hash: hash.clone(),
        });
    }

    extend_embedding_closure(
        repo,
        &targets,
        &stored_chunks,
        &surviving_text,
        &embeddings,
        &mut items,
        &mut preserved,
    )?;
    PurgeClosure::new(purge_id.to_owned(), items, preserved)
}

struct PurgeEmbedding {
    hash: String,
    target_type: String,
    target_hash: String,
}

/// Keep only bounded identity metadata, never all vector payloads in memory.
fn purge_embedding_inventory(repo: &Repository) -> Result<Vec<PurgeEmbedding>> {
    let store = repo.object_store();
    store
        .embedding_hashes()?
        .into_iter()
        .map(|hash| {
            let object = store.read_embedding(&hash)?;
            Ok(PurgeEmbedding {
                hash,
                target_type: object.target_type,
                target_hash: object.target_hash,
            })
        })
        .collect()
}

/// Freeze vector identity authority before either the chunk ledger or SQLite
/// is mutated. A retained semantic CAS inventory also covers vectors whose
/// disposable SQLite row is already absent at preparation time.
fn extend_embedding_closure(
    repo: &Repository,
    targets: &BTreeSet<&str>,
    stored_chunks: &[crate::StoredChunk],
    surviving_text: &BTreeSet<String>,
    embeddings: &[PurgeEmbedding],
    items: &mut Vec<ClosureItem>,
    preserved: &mut Vec<ClosureItem>,
) -> Result<()> {
    let target_chunks = stored_chunks
        .iter()
        .filter(|stored| targets.contains(stored.row.raw_hash.as_str()))
        .map(|stored| (stored.row.chunk_id.as_str(), &stored.row))
        .collect::<BTreeMap<_, _>>();
    let target_text = target_chunks
        .values()
        .map(|row| row.text_hash.clone())
        .collect::<BTreeSet<_>>();
    let ledger_chunks = stored_chunks
        .iter()
        .map(|stored| (stored.row.chunk_id.as_str(), &stored.row))
        .collect::<BTreeMap<_, _>>();
    let sqlite = crate::sqlite_path(repo.kio_dir());
    if path_exists(&sqlite)? {
        let index = open_existing_source_index_connection(
            &sqlite,
            ExistingSourceIndexOpenMode::ReadOnly,
            &FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            },
        )
        .map_err(crate::index_to_kio)?;
        // Match the existing closure's item ceiling. Stream bounded metadata;
        // SQLite rows must agree with the ledger and confer no ownership.
        let limit = kio_core::purge::MAX_PURGE_TARGETS.saturating_mul(8);
        let mut statement = index
            .prepare("SELECT chunk_id, raw_hash, text_hash FROM chunks LIMIT ?1")
            .map_err(|error| KioError::schema(error.to_string()))?;
        let rows = statement
            .query_map([limit as i64 + 1], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| KioError::schema(error.to_string()))?;
        for (position, row) in rows.enumerate() {
            if position >= limit {
                return Err(purge_authority_incomplete(
                    "chunk reference inventory exceeds the purge bound",
                ));
            }
            let (chunk_id, raw_hash, text_hash) =
                row.map_err(|error| KioError::schema(error.to_string()))?;
            let Some(ledger) = ledger_chunks.get(chunk_id.as_str()) else {
                return Err(purge_authority_incomplete(
                    "index chunk has no ledger owner",
                ));
            };
            if ledger.raw_hash != raw_hash || ledger.text_hash != text_hash {
                return Err(purge_authority_incomplete(
                    "index chunk disagrees with its ledger owner",
                ));
            }
        }
        index
            .recheck_source_identity()
            .map_err(crate::index_to_kio)?;
    }
    let removable_images = items
        .iter()
        .filter(|item| item.object_type == "image")
        .map(|item| item.hash.clone())
        .collect::<BTreeSet<_>>();
    let shared_images = preserved
        .iter()
        .filter(|item| item.object_type == "image")
        .map(|item| item.hash.clone())
        .collect::<BTreeSet<_>>();
    for object in embeddings {
        let (target, shared) = match object.target_type.as_str() {
            "chunk" => (
                target_text.contains(&object.target_hash),
                surviving_text.contains(&object.target_hash),
            ),
            "image" => (
                removable_images.contains(&object.target_hash)
                    || shared_images.contains(&object.target_hash),
                shared_images.contains(&object.target_hash),
            ),
            _ => continue,
        };
        if target {
            let item = ClosureItem {
                object_type: "embedding".to_owned(),
                hash: object.hash.clone(),
            };
            if shared {
                preserved.push(item);
            } else {
                items.push(item);
            }
        }
    }
    Ok(())
}

fn purge_authority_incomplete(message: &str) -> KioError {
    KioError::new(
        "KIO-E-PURGE-AUTHORITY-INCOMPLETE-001",
        format!(
            "{message}; reconcile the chunk ledger, immutable history and CAS before retrying purge. kio repair rebuild-db can repair only the SQLite projection of intact authenticated records; it cannot restore missing attribution"
        ),
        json!({}),
        ExitCode::PermanentFailure,
    )
}

/// `chunks.jsonl` is an acceleration ledger, not deletion authority.  Before a
/// target-associated row can contribute a chunk id to a purge closure, prove
/// both the row and the CAS body against units pinned by immutable retained
/// commit/tree history.  In particular, never trust its raw hash merely to
/// select a chunk object for deletion.
fn authenticate_purge_chunk_inventory(
    repo: &Repository,
    targets: &BTreeSet<&str>,
    stored_chunks: &[crate::StoredChunk],
    embeddings: &[PurgeEmbedding],
) -> Result<BTreeSet<String>> {
    let retained = crate::retained_history_instances_for_roots(
        repo.kio_dir(),
        &crate::durable_history_roots(repo)?,
    )?;
    let authorities = crate::retained_unit_introductions(repo, &retained)?;
    let store = repo.object_store();
    let target_text = stored_chunks
        .iter()
        .filter(|stored| targets.contains(stored.row.raw_hash.as_str()))
        .map(|stored| stored.row.text_hash.clone())
        .collect::<BTreeSet<_>>();
    let vector_text = embeddings
        .iter()
        .filter(|object| object.target_type == "chunk")
        .map(|object| object.target_hash.clone())
        .collect::<BTreeSet<_>>();
    let relevant_text = target_text
        .union(&vector_text)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut relevant = BTreeMap::new();
    let mut surviving_text = BTreeSet::new();
    for stored in stored_chunks {
        let row = &stored.row;
        let target = targets.contains(row.raw_hash.as_str());
        if !target && !relevant_text.contains(&row.text_hash) {
            continue;
        }
        crate::authenticate_chunk_row(row, &authorities)
            .map_err(chunk_ledger_authentication_error)?;
        if !target && crate::purge_blocks_rebuild_raw(repo.kio_dir(), &row.raw_hash)? {
            return Err(purge_authority_incomplete(
                "relevant survivor has a blocking purge lifecycle",
            ));
        }
        let expected = ChunkObject {
            spec_version: 1,
            raw_hash: row.raw_hash.clone(),
            tool_profile_hash: row.tool_profile_hash.clone(),
            r#gen: row.r#gen,
            unit_key: row.unit_key.clone(),
            unit_content_hash: row.unit_content_hash.clone(),
            heading_path: row.heading_path.clone().unwrap_or_default(),
            section_id: stored
                .row
                .section_id
                .clone()
                .filter(|value| !value.is_empty()),
            byte_start: row.byte_start,
            byte_end: row.byte_end,
            text_hash: row.text_hash.clone(),
            text: row.text.clone(),
        };
        if let Some(previous) = relevant.insert(row.chunk_id.clone(), expected.clone())
            && previous != expected
        {
            return Err(purge_authority_incomplete(
                "chunk ledger has conflicting identities",
            ));
        }
        if !target {
            surviving_text.insert(row.text_hash.clone());
        }
    }
    let mut seen = BTreeSet::new();
    let mut proven_text = BTreeSet::new();
    store.visit_chunks_bounded(|hash, actual| {
        if targets.contains(actual.raw_hash.as_str()) || relevant_text.contains(&actual.text_hash) {
            let Some(expected) = relevant.get(hash) else {
                return Err(purge_authority_incomplete(
                    "relevant chunk CAS has no authenticated ledger/history attribution",
                ));
            };
            if actual != expected {
                return Err(purge_authority_incomplete(
                    "chunk CAS disagrees with its authenticated ledger owner",
                ));
            }
            seen.insert(hash.to_owned());
            proven_text.insert(actual.text_hash.clone());
        }
        Ok(())
    })?;
    if relevant.keys().any(|hash| !seen.contains(hash)) {
        return Err(purge_authority_incomplete(
            "authenticated relevant chunk is missing its canonical CAS body",
        ));
    }
    // A vector carries a text identity, not a raw owner. Missing all chunk
    // attribution cannot justify either deletion or an unrelated classification.
    // Refuse globally so loss of ledger + index + chunk CAS cannot hide target
    // plaintext in a surviving embedding while purge reports completion.
    if vector_text.iter().any(|hash| !proven_text.contains(hash)) {
        return Err(purge_authority_incomplete(
            "chunk-target embedding has no authenticated canonical chunk attribution",
        ));
    }
    Ok(surviving_text)
}

fn chunk_ledger_authentication_error(error: KioError) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "chunk ledger creation is not authenticated by immutable history",
        json!({ "reason": error.to_string() }),
        ExitCode::PermanentFailure,
    )
}

/// Run the durable phase machine. This is kept separate from preview so there
/// is no mutation before confirmation and so an under-lock target re-plan can be
/// compared byte-for-byte with the previewed authority.
///
/// LC46-LC51: the journal's phase vocabulary is `prepared -> tombstoned ->
/// deleted -> committed`, then `done` (journal removed). `prepared` fixes
/// `purge_id`/`target_epoch`/`planned_commit`/`closure` once — the last two
/// via a non-publishing dry-run snapshot (`Repository::purged_snapshot(...,
/// publish_ref=false)`, LC48) — *before* any marker is durable, so the
/// tombstone/erase-receipt's `in_commit` can reference the eventual purged
/// commit's hash while it still only exists content-addressed in the CAS
/// (not yet ref-reachable). `tombstoned` publishes that marker — durable
/// *before* any physical deletion (LC49). `deleted` performs the physical
/// deletion. `committed` re-derives the identical commit (content-addressed,
/// deterministic — the store lock has been held throughout) and publishes it
/// for real, making it ref-reachable for the first time.
fn execute_phase_machine(
    repo: &Repository,
    args: &PurgeArgs,
    preview: &PurgePreview,
) -> Result<Value> {
    let _store_lock = repo.lock_store()?;
    let locked_plan = resolve_plan(repo, args)?;
    if locked_plan != preview.plan {
        return Err(KioError::new(
            "KIO-E-PURGE-PLAN-CHANGED-001",
            "purge target changed between preview and the locked recheck",
            json!({ "target_raw_count": preview.plan.target_raw_hashes.len() }),
            ExitCode::PartialFailure,
        ));
    }
    // Serialize the visibility barrier and destructive phases against restore's
    // final source recheck/staging/publication window. Lock order is always scope
    // store -> purge publication; restore takes only the latter.
    let _publication_lock = repo.lock_purge_publication()?;
    // Re-run after reacquiring the store lock: a prior Kio writer could have
    // crashed between preview cleanup and this confirmed phase-machine entry.
    cleanup_orphan_raw_ingest_temps(repo.kio_dir())?;

    let state = PurgeState::open(repo.kio_dir())?;
    let active = state.read_journal()?;
    // PA37-39 (§K, U34; §R ruling #1): a working-tree residual of the exact
    // same bytes is a WARNING, never a purge-blocking hard failure — "Kio
    // does not delete the user's files" cuts the other way here: purge must
    // still complete (05 §3.5 L741 "working tree の原本には触れない" — this
    // check exists to warn, not to hold the object-store side hostage to a
    // working-tree file purge is contractually forbidden from touching
    // anyway). §R ruling #1 explicitly retires the prior
    // `KIO-E-PURGE-WORKING-COPY-001` hard block. No same-path/renamed-alias
    // distinction (ruling #1) — `detect_live_working_copy` already matches by
    // raw_hash content identity regardless of the alias's current name.
    let working_tree_alias_count = detect_live_working_copy(repo, &locked_plan.target_raw_hashes)?;
    let completed = if active.is_none() {
        inspect_terminal_state(&state, &locked_plan)?
    } else {
        // Terminal records precede physical deletion. Their presence cannot
        // complete a transaction while its resumable journal still exists.
        inspect_terminal_state(&state, &locked_plan)?;
        None
    };
    verify_targets_exist(repo, &locked_plan, active.is_some(), completed.is_some())?;
    if let Some(completed) = completed {
        return Ok(attach_working_tree_warning(
            completed_report(&locked_plan, completed),
            working_tree_alias_count,
        ));
    }

    // A resumed journal (matching target/reason/mode) reuses every `prepared`
    // field verbatim (LC48/LC50: fixed once, never recomputed). Only a fresh
    // start computes new ones — `target_epoch` from the current purge-epoch
    // counter (recovered if missing, LC40) plus one, and `planned_commit` via
    // the non-publishing dry-run snapshot above.
    let resuming = active.as_ref().is_some_and(|journal| {
        journal.target_raw_hashes == {
            let mut sorted = locked_plan.target_raw_hashes.clone();
            sorted.sort();
            sorted.dedup();
            sorted
        } && journal.reason == locked_plan.reason
            && journal.tombstone_mode == locked_plan.tombstone_mode
    });
    let (started_at, target_epoch, planned_commit, closure_hash, purge_id) = if resuming {
        let journal = active.as_ref().expect("resuming implies active.is_some()");
        (
            journal.started_at.clone(),
            journal.target_epoch,
            journal.planned_commit.clone(),
            // PA44: never recomputed on resume — the sidecar
            // `state.begin` below is about to (re-)discover was already
            // written durably before this journal was ever created.
            journal.closure_hash.clone(),
            journal.purge_id.clone(),
        )
    } else {
        let started_at = now_utc_seconds();
        let recovery_target = state
            .max_recorded_purge_epoch()?
            .map_or(1, |max| max.saturating_add(1));
        let current_epoch = state.ensure_purge_epoch(recovery_target)?;
        let target_epoch = current_epoch.saturating_add(1);
        let dry_run = repo.purged_snapshot(
            &locked_plan.reason.to_string(),
            Some(&started_at),
            &locked_plan.target_raw_hashes,
            false,
        )?;
        let planned_commit = dry_run.commit_hash.ok_or_else(|| {
            KioError::new(
                "KIO-E-STORE-CORRUPT-001",
                "purged snapshot dry run did not return a planned commit hash",
                json!({}),
                ExitCode::PermanentFailure,
            )
        })?;
        let purge_id = kio_core::scope::new_ulid(repo.kio_dir());
        // PA43/44 (§R ruling #2): compute + durably write the closure sidecar
        // BEFORE `state.begin` below creates the journal that references its
        // content hash — so the journal can never point at a closure that
        // is not yet durable.
        let closure = compute_purge_closure(repo, &locked_plan, &purge_id)?;
        state.write_closure(&closure)?;
        let closure_hash = closure_content_hash(&closure)?;
        (
            started_at,
            target_epoch,
            planned_commit,
            closure_hash,
            purge_id,
        )
    };

    let (mut journal, newly_started) = match state.begin(
        locked_plan.target_raw_hashes.clone(),
        locked_plan.reason,
        locked_plan.tombstone_mode,
        current_actor(),
        started_at,
        target_epoch,
        planned_commit,
        closure_hash,
        purge_id,
    )? {
        BeginOutcome::Started(journal) => (journal, true),
        BeginOutcome::Resumed(journal) => (journal, false),
        BeginOutcome::AlreadyComplete(tombstones) => {
            let completed = CompletedTerminal {
                purged_in_commit: tombstones
                    .first()
                    .map(|record| record.tail().in_commit.clone())
                    .unwrap_or_default(),
                tombstone_count: u64::try_from(tombstones.len()).unwrap_or(u64::MAX),
                erase_receipt_count: 0,
            };
            return Ok(attach_working_tree_warning(
                completed_report(&locked_plan, completed),
                working_tree_alias_count,
            ));
        }
    };

    if let Err(error) = maybe_inject_fault("prepared") {
        if newly_started {
            state.abort_before_barrier(&journal)?;
        }
        return Err(error);
    }

    // PA44/45: read the closure sidecar back and bind it to the journal by
    // content hash before trusting its contents for any destructive
    // decision — on both the fresh-start and resumed paths alike, so a
    // resumed `deleted` phase reuses exactly the same removable/preserved
    // determination the original `prepared` phase fixed, never a live
    // rescan (`delete_derived_surfaces` below consumes this, not
    // `scan_derived_inventory` directly, for its shared-vs-removable split).
    let closure = state.read_closure()?.ok_or_else(|| {
        KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "purge journal references a closure sidecar that does not exist",
            json!({ "closure_hash": journal.closure_hash }),
            ExitCode::PermanentFailure,
        )
    })?;
    if closure_content_hash(&closure)? != journal.closure_hash {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "purge closure sidecar content hash does not match the journal's closure_hash",
            json!({ "closure_hash": journal.closure_hash }),
            ExitCode::PermanentFailure,
        ));
    }

    let mut report = load_phase_report(repo.kio_dir())?.unwrap_or_default();
    let result = execute_visible_phases(
        repo,
        &state,
        &locked_plan,
        &closure,
        &mut journal,
        &mut report,
    );
    match result {
        Ok(()) => {
            // PC20 (05 §1.5 L180-184): purge is one of the 6 listed
            // `index_generation` rotation triggers in its own right (deleted
            // chunk/config/vector rows change the search-visible set,
            // independent of whether this purge also happened to move the
            // lifecycle-epoch counter) — rotate unconditionally first.
            crate::rotate_index_generation_unconditionally(repo.kio_dir())?;
            // LC42-LC44 (item 2): purge's own `tombstoned`-phase marker
            // append (and any resurrection retire folded into the same
            // mutation) advances `.kio/tombstones/lifecycle-epoch` directly
            // (unlike `kio index`/`reindex`/`repair rebuild-db`, purge
            // writes `sqlite.db` in place — `delete_derived_surfaces`'s
            // `SqliteFtsIndex::open` on the live path, no temp+rename — so
            // there is no later rename to discard this write). Without this,
            // `index_metadata.last_lifecycle_epoch` goes stale the moment
            // this purge completes and LC45's read-side check would reject
            // every read command until an unrelated index-touching write
            // happened to run.
            crate::recover_index_generation(repo.kio_dir())?;
            // 05 §1.8 write-through. Ranking is not the reason here: the
            // rotation makes an incomplete replica fail closed; it does not
            // ask a reader to re-project this scope. The
            // reason is that the replica holds the chunk TEXT, on the device,
            // under the cache root — and purge exists to make that text stop
            // existing. Waiting for a reader to notice would leave purged
            // content readable in `aggregator.sqlite` for as long as the
            // writer had not published a coherent deletion.
            //
            // A full re-projection rather than a delta, deliberately: purge
            // deletes chunk rows, config associations, chunk vectors AND
            // orphaned embeddings, and it is rare and already expensive. Being
            // exactly right about the surviving set matters more here than
            // saving the milliseconds a delta would.
            //
            // R25-5: and this is the one caller that FAILS on a lost cache
            // write. Everywhere else the replica is a cache and a cache may not
            // break a command (03 §4). Here the "cache" is a second copy of the
            // text the user asked to stop existing, so reporting success while
            // it is still readable under the cache root is not a degraded
            // result — it is a false one.
            crate::write_through_projection(repo.kio_dir()).map_err(|reason| {
                KioError::new(
                    "KIO-E-PURGE-REPLICA-001",
                    "purge removed the content but could not remove it from the \
                     device search replica; re-run purge, or delete \
                     `aggregator.sqlite` under the cache root",
                    json!({ "reason": reason }),
                    ExitCode::Failure,
                )
            })?;
            Ok(attach_working_tree_warning(
                success_report(&locked_plan, &report),
                working_tree_alias_count,
            ))
        }
        Err(error) => Ok(attach_working_tree_warning(
            incomplete_report(&locked_plan, &journal, &report, &error),
            working_tree_alias_count,
        )),
    }
}

fn execute_visible_phases(
    repo: &Repository,
    state: &PurgeState,
    plan: &PurgePlan,
    closure: &PurgeClosure,
    journal: &mut PurgeJournal,
    report: &mut PurgeReport,
) -> Result<()> {
    maybe_inject_fault("prepared_visible")?;

    if journal.phase == PurgePhase::Prepared {
        // PA40/41 (§L, U37): settle in-flight/orphaned-cleanup ledger
        // reservations for this purge's own scope+targets BEFORE the
        // tombstone/erase-receipt is published (let alone physical deletion
        // or commit publish) — strictly within the `prepared` phase, same as
        // `publish_terminal_records` below but ordered first. Idempotent on
        // resume: a row already settled by an earlier pass through this
        // block is simply no longer in-flight/cleanup-pending the second
        // time, so it is skipped rather than re-settled.
        settle_inflight_reservations_for_purge(repo, &plan.target_raw_hashes, report)?;
        // LC49: the marker is durable *before* any physical deletion, using
        // the already-fixed `planned_commit` (the purged commit is not yet
        // ref-published — see the module doc on `execute_phase_machine`).
        publish_terminal_records(state, journal, report)?;
        *journal = state.advance_phase(journal, PurgePhase::Tombstoned)?;
        store_phase_report(repo.kio_dir(), report)?;
    }
    maybe_inject_fault("tombstoned")?;

    if journal.phase == PurgePhase::Tombstoned {
        delete_content_surfaces(repo, plan, closure, report)?;
        delete_derived_surfaces(repo, plan, closure, report)?;
        scrub_logs(repo, plan, journal, report)?;
        *journal = state.advance_phase(journal, PurgePhase::Deleted)?;
        store_phase_report(repo.kio_dir(), report)?;
    }
    maybe_inject_fault("deleted")?;

    if journal.phase == PurgePhase::Deleted {
        let commit_hash = publish_planned_commit(repo, plan, journal)?;
        verify_purged_commit(repo, &commit_hash, &plan.target_raw_hashes)?;
        report.purged_in_commit = Some(commit_hash);
        *journal = state.advance_phase(journal, PurgePhase::Committed)?;
        store_phase_report(repo.kio_dir(), report)?;
    }
    maybe_inject_fault("committed")?;

    // Final scrub closes the append race before the visibility barrier is
    // removed. The audit row is identifier-free and is appended while both scrub
    // locks are still covered by `scrub_logs`' own serialized pass.
    finalize_purge(repo, state, plan, journal, report)?;
    Ok(())
}

/// `committed` phase: re-derive (deterministic, content-addressed — the
/// working tree cannot have changed under the held store lock) and publish
/// for real the commit whose hash was already fixed as `journal.planned_commit`
/// at `prepared`. A mismatch is `KIO-E-STORE-CORRUPT-001` (defense-in-depth;
/// the only legitimate cause would be an external, non-KIO-mediated edit to
/// the working tree between `prepared` and `committed`, which the store lock
/// cannot prevent).
fn publish_planned_commit(
    repo: &Repository,
    plan: &PurgePlan,
    journal: &PurgeJournal,
) -> Result<String> {
    let outcome = repo.purged_snapshot(
        &plan.reason.to_string(),
        Some(&journal.started_at),
        &plan.target_raw_hashes,
        true,
    )?;
    if outcome.noop
        || outcome.commit.as_ref().map(|commit| commit.commit_type) != Some(CommitType::Purged)
    {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "purged snapshot did not create a protected commit",
            json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    let commit_hash = outcome.commit_hash.ok_or_else(|| {
        KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "purged snapshot did not return its commit hash",
            json!({}),
            ExitCode::PermanentFailure,
        )
    })?;
    if commit_hash != journal.planned_commit {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "purged commit hash diverged from the journal's planned_commit",
            json!({ "planned_commit": journal.planned_commit, "actual": commit_hash }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(commit_hash)
}

fn verify_purged_commit(repo: &Repository, commit_hash: &str, targets: &[String]) -> Result<()> {
    let commit = repo.read_commit(commit_hash)?;
    if commit.commit_type != CommitType::Purged {
        return Err(KioError::schema(
            "purge journal commit is not commit_type=purged",
        ));
    }
    let tree = repo.read_tree(&commit.tree)?;
    let targets = targets.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if tree
        .entries
        .iter()
        .any(|entry| targets.contains(entry.raw_hash.as_str()))
    {
        return Err(KioError::new(
            "KIO-E-PURGE-WORKING-COPY-001",
            "purged commit still contains target bytes",
            json!({}),
            ExitCode::PartialFailure,
        ));
    }
    Ok(())
}

/// `prepared -> tombstoned`: append `purged`/`erased` to each target's marker,
/// referencing `journal.planned_commit` (LC49). Idempotent on resume
/// (`PurgeState::append_*_event`'s `events_are_equivalent` check).
fn publish_terminal_records(
    state: &PurgeState,
    journal: &PurgeJournal,
    report: &mut PurgeReport,
) -> Result<()> {
    for raw_hash in &journal.target_raw_hashes {
        match journal.tombstone_mode {
            TombstoneMode::Default => {
                let event = LifecycleEvent::purged(
                    journal.started_at.clone(),
                    journal.planned_commit.clone(),
                    journal.reason,
                    journal.actor.clone(),
                    journal.target_epoch,
                );
                state.append_tombstone_event(raw_hash, event)?;
                report.tombstone_count =
                    u64::try_from(journal.target_raw_hashes.len()).unwrap_or(u64::MAX);
            }
            TombstoneMode::Erase => {
                let event = LifecycleEvent::erased(
                    journal.started_at.clone(),
                    journal.planned_commit.clone(),
                    journal.reason,
                    journal.actor.clone(),
                    journal.target_epoch,
                );
                state.append_erase_receipt_event(raw_hash, event)?;
                report.erase_receipt_count =
                    u64::try_from(journal.target_raw_hashes.len()).unwrap_or(u64::MAX);
            }
        }
    }
    Ok(())
}

fn delete_content_surfaces(
    repo: &Repository,
    plan: &PurgePlan,
    closure: &PurgeClosure,
    report: &mut PurgeReport,
) -> Result<()> {
    let targets = plan
        .target_raw_hashes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    // PA43/46: the target chunk_id set is fixed in the closure at `prepared`
    // (raw-hash membership, no cross-purge drift risk) rather than rescanned
    // from `chunks.jsonl` here on every entry (fresh or resumed).
    let chunk_ids = closure.hashes_for("chunk");
    store_phase_chunk_ids(
        repo.kio_dir(),
        &chunk_ids.iter().cloned().collect::<Vec<_>>(),
    )?;

    let store = repo.object_store();
    for raw_hash in &plan.target_raw_hashes {
        if store.remove_raw(raw_hash)? {
            report.deleted.raw_objects = report.deleted.raw_objects.saturating_add(1);
        }
    }
    for chunk_id in &chunk_ids {
        if store.remove_chunk(chunk_id)? {
            report.deleted.chunk_objects = report.deleted.chunk_objects.saturating_add(1);
        }
    }

    let stored = crate::read_stored_chunks(repo.kio_dir())?;
    let kept = stored
        .into_iter()
        .filter(|stored| !targets.contains(stored.row.raw_hash.as_str()))
        .collect::<Vec<_>>();
    report.deleted.chunk_ledger_rows = rewrite_chunk_ledger(repo, &kept)?;
    Ok(())
}

fn rewrite_chunk_ledger(repo: &Repository, kept: &[crate::StoredChunk]) -> Result<u64> {
    let kio_dir = repo.kio_dir();
    let original_count = crate::read_stored_chunks(kio_dir)?.len();
    let mut bytes = Vec::new();
    for stored in kept {
        serde_json::to_writer(&mut bytes, stored)
            .map_err(|error| KioError::schema(error.to_string()))?;
        bytes.push(b'\n');
    }
    // Preserve non-target publication events; target events are removed with
    // their creation rows. Re-emit from the durable ledger after filtering.
    let kept_ids = kept
        .iter()
        .map(|c| c.row.chunk_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for event in authenticated_publication_events(repo, kept)? {
        if kept_ids.contains(event.chunk_id.as_str()) {
            serde_json::to_writer(&mut bytes, &event)
                .map_err(|e| KioError::schema(e.to_string()))?;
            bytes.push(b'\n');
        }
    }
    // The chunk ledger is repository state, not purge-private state.  Rewrite
    // it through the retained `.kio/index` capability so a concurrent public
    // path replacement cannot redirect the replacement into an attacker-owned
    // directory.
    crate::replace_chunk_ledger_contents(kio_dir, &bytes)?;
    Ok(u64::try_from(original_count.saturating_sub(kept.len())).unwrap_or(u64::MAX))
}

/// Publication rows are liveness hints only after their introduction commit
/// attests the exact unit for the creation association.  A missing commit is
/// tolerated as the established pre-commit crash residue; a present but forged
/// event remains corruption and is never carried through a purge rewrite.
fn authenticated_publication_events(
    repo: &Repository,
    creations: &[crate::StoredChunk],
) -> Result<Vec<crate::ChunkPublicationEvent>> {
    let creations_by_association = creations
        .iter()
        .map(|creation| {
            (
                (
                    creation.row.chunk_id.clone(),
                    creation.row.chunking_config_hash.clone(),
                ),
                creation,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut authenticated = Vec::new();
    let mut authentication_cache = crate::PublicationAuthenticationCache::default();
    for event in crate::read_chunk_publication_events(repo.kio_dir())? {
        let Some(creation) = creations_by_association
            .get(&(event.chunk_id.clone(), event.chunking_config_hash.clone()))
        else {
            // Its creation is being purged, so the event cannot contribute to
            // preserved liveness or the rewritten ledger.
            continue;
        };
        if crate::authenticate_publication_event_cached(
            repo,
            repo.kio_dir(),
            &event,
            creation,
            &mut authentication_cache,
        )? {
            authenticated.push(event);
        }
    }
    Ok(authenticated)
}

fn phase_chunk_ids_path(kio_dir: &Path) -> PathBuf {
    kio_dir.join("purge/chunk-ids.json")
}

fn store_phase_chunk_ids(kio_dir: &Path, chunk_ids: &[String]) -> Result<()> {
    let bytes =
        serde_json::to_vec(chunk_ids).map_err(|error| KioError::schema(error.to_string()))?;
    atomic_private_replace(&phase_chunk_ids_path(kio_dir), &bytes)
}

fn phase_report_path(kio_dir: &Path) -> PathBuf {
    kio_dir.join("purge/report.json")
}

fn store_phase_report(kio_dir: &Path, report: &PurgeReport) -> Result<()> {
    let bytes = serde_json::to_vec(report).map_err(|error| KioError::schema(error.to_string()))?;
    atomic_private_replace(&phase_report_path(kio_dir), &bytes)
}

fn load_phase_report(kio_dir: &Path) -> Result<Option<PurgeReport>> {
    let path = phase_report_path(kio_dir);
    let Some(bytes) = read_bounded_regular(&path, 1024 * 1024)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "purge report sidecar is corrupt",
            json!({ "path": path.display().to_string(), "message": error.to_string() }),
            ExitCode::PermanentFailure,
        )
    })
}

#[derive(Debug, Default)]
struct DerivedInventory {
    target_prepared: BTreeSet<String>,
    target_images: BTreeSet<String>,
    surviving_prepared: BTreeSet<String>,
    surviving_images: BTreeSet<String>,
    /// PB04/item 3 follow-through (05 §3.5 L701-703 "normalized は...manifest
    /// object...を含む"): every target instance's manifest CAS object hash
    /// (`objects/manifests/` — never shared across raws, unlike
    /// prepared/image, so no surviving/target split is needed). Absent from
    /// this set for raw-only entries, which have no normalized instance.
    target_manifest_hashes: BTreeSet<String>,
    target_normalized_units: BTreeSet<String>,
    surviving_normalized_units: BTreeSet<String>,
}

fn delete_derived_surfaces(
    repo: &Repository,
    plan: &PurgePlan,
    closure: &PurgeClosure,
    report: &mut PurgeReport,
) -> Result<()> {
    let targets = plan
        .target_raw_hashes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let store = repo.object_store();

    // SQLite owns derived rows and counts. Every CAS target was authenticated
    // and frozen before these rows could disappear; DELETE RETURNING IDs must
    // never become new removal authority or the only crash-recovery inventory.
    let sqlite = crate::sqlite_path(repo.kio_dir());
    let chunk_ids = closure.hashes_for("chunk");
    if path_exists(&sqlite)? {
        validate_existing_regular(&sqlite, None)?;
        let mut index = SqliteFtsIndex::open(
            &sqlite,
            FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            },
        )
        .map_err(crate::index_to_kio)?;
        // The journal-bound closure fixes image liveness from authenticated,
        // immutable unit ownership. Markdown/metadata and mutable chunk rows
        // can cite an image but cannot establish a local CAS lifecycle edge.
        let orphaned_images = closure.hashes_for("image");
        let no_images = BTreeSet::new();
        for (index_position, raw_hash) in plan.target_raw_hashes.iter().enumerate() {
            let deleted = index
                .purge_raw(
                    raw_hash,
                    if index_position == 0 {
                        &orphaned_images
                    } else {
                        &no_images
                    },
                )
                .map_err(crate::index_to_kio)?;
            report.deleted.sqlite_chunks = report
                .deleted
                .sqlite_chunks
                .saturating_add(deleted.deleted_chunks);
            report.deleted.sqlite_associations = report
                .deleted
                .sqlite_associations
                .saturating_add(deleted.deleted_associations);
            // Both vec0 tables, in one counter. `PurgeRawIndexReport` keeps them
            // apart because the two are DECIDED by different rules (a chunk
            // vector goes with its chunk; an image vector only when no surviving
            // chunk still cites the image), but an operator reading this report
            // wants "how many vector rows stopped existing", and both are that.
            // Counting only the chunk half understated the purge.
            report.deleted.sqlite_vectors = report
                .deleted
                .sqlite_vectors
                .saturating_add(deleted.deleted_chunk_vectors)
                .saturating_add(deleted.deleted_image_vectors);
            report.deleted.sqlite_orphan_embeddings = report
                .deleted
                .sqlite_orphan_embeddings
                .saturating_add(deleted.deleted_orphan_embeddings);
        }
        drop(index);
    }
    // Retry every frozen identity even if SQLite committed its DELETE before
    // the previous process stopped at a CAS quarantine transition.
    for embedding_id in closure.hashes_for("embedding") {
        store.remove_embedding(&embedding_id)?;
    }
    for chunk_id in &chunk_ids {
        if store.remove_chunk(chunk_id)? {
            report.deleted.chunk_objects = report.deleted.chunk_objects.saturating_add(1);
        }
    }

    // PA44 (§N, §R ruling #2): the shared-vs-removable live-reference
    // judgment for `prepared`/`image` is fixed once, in the closure, at
    // `prepared` — read back here (both fresh and resumed) rather than
    // recomputed via a live `scan_derived_inventory` diff, so a crash-resume
    // cannot let an intervening `kio index`/other purge change which objects
    // this purge deletes (the exact scenario PA44 fixes). Cache directories
    // are located independently by canonical target-raw paths, so recovery
    // never depends on immutable objects this purge has already removed.
    let removable_prepared = closure.hashes_for("prepared");
    let removable_images = closure.hashes_for("image");
    let removable_normalized_units = closure.hashes_for("normalized_unit");
    report.shared.prepared_objects =
        u64::try_from(closure.preserved_hashes_for("prepared").len()).unwrap_or(u64::MAX);
    report.shared.image_objects =
        u64::try_from(closure.preserved_hashes_for("image").len()).unwrap_or(u64::MAX);
    report.shared.normalized_unit_objects =
        u64::try_from(closure.preserved_hashes_for("normalized_unit").len()).unwrap_or(u64::MAX);

    let store = repo.object_store();
    for hash in &removable_prepared {
        if store.remove_content(ContentObjectKind::Prepared, hash)? {
            report.deleted.prepared_objects = report.deleted.prepared_objects.saturating_add(1);
        }
    }
    for hash in &removable_images {
        if store.remove_content(ContentObjectKind::Image, hash)? {
            report.deleted.image_objects = report.deleted.image_objects.saturating_add(1);
        }
    }
    // The closure was derived from verified immutable manifest pins before
    // the journal was created. Verify the unit CAS body again immediately
    // before unlinking it.
    for hash in &removable_normalized_units {
        if store.remove_content(ContentObjectKind::NormalizedUnit, hash)? {
            report.deleted.normalized_unit_objects =
                report.deleted.normalized_unit_objects.saturating_add(1);
        }
    }
    // PB04/item 3 (05 §3.5 L701-703): the target instances' own manifest CAS
    // objects, fixed in the closure at `prepared` like prepared/image above —
    // never shared, so no live-reference check is needed here, only the
    // idempotent removal itself.
    for hash in closure.hashes_for("manifest") {
        if store.remove_content(ContentObjectKind::Manifest, &hash)? {
            report.deleted.manifest_objects = report.deleted.manifest_objects.saturating_add(1);
        }
    }

    report.deleted.normalized_instances = report
        .deleted
        .normalized_instances
        .saturating_add(remove_target_normalized_instances(repo, &targets)?);
    report.deleted.normalized_views = remove_target_normalized_views(repo.kio_dir(), &targets)?;

    delete_target_tasks(repo, &targets, &chunk_ids, report)?;
    report.deleted.manifest_rows = scrub_manifest(repo.kio_dir(), &targets)?;
    report.deleted.unsupported_rows = scrub_unsupported(repo.kio_dir(), &targets)?;
    report.deleted.quarantine_rows = scrub_quarantine(repo.kio_dir(), &plan.historical_paths)?;
    report.deleted.staging_descriptors = delete_target_staging(repo.kio_dir(), &targets)?;

    // PA03/PA12/PA13: `image` cache directories live under a distinct
    // `open/image/<hash>/` type segment (raw/prepared share the flat
    // `open/<hash>/` namespace they always have) — the eviction side must
    // mirror the same split `open_cache_path`/`open_cas_byte_object` use on
    // the materialize side, or a same-digest raw/image pair's cache entries
    // collide and an eviction for one wrongly removes the other's cache.
    let mut flat_cache_hashes = plan
        .target_raw_hashes
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    flat_cache_hashes.extend(removable_prepared);
    report.deleted.cache_directories = evict_open_cache(&flat_cache_hashes, false)?
        .saturating_add(evict_open_cache(&removable_images, true)?);
    Ok(())
}

/// Cache-instance ownership is encoded in the canonical
/// `<raw>.<tool>.g<generation>` child of the raw's exact two-level fanout.
/// Prepared already authenticated the raw targets and immutable deletion
/// closure. Recovery must not reread a manifest/unit CAS body that this same
/// purge may have removed, nor require a cache manifest after partial cleanup.
#[cfg(any(unix, windows))]
fn remove_target_normalized_instances(repo: &Repository, targets: &BTreeSet<&str>) -> Result<u64> {
    use kio_core::store_dir::StoreDirectory;
    fn child(parent: &StoreDirectory, name: &str) -> Result<Option<StoreDirectory>> {
        match cap_primitives::fs::open_dir_nofollow(&parent.root_handle(), Path::new(name)) {
            Ok(handle) => StoreDirectory::from_retained(handle, parent.path().join(name)).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(KioError::io(
                error.to_string(),
                parent.path().join(name).display().to_string(),
            )),
        }
    }
    let handle = repo.bound_kio_handle().ok_or_else(|| {
        KioError::invalid_usage("purge cache cleanup requires retained .kio authority")
    })?;
    let mut base = StoreDirectory::from_retained(
        handle
            .try_clone()
            .map_err(|error| KioError::io(error.to_string(), "purge .kio handle"))?,
        repo.kio_dir().to_path_buf(),
    )?;
    for name in ["objects", "normalized_units"] {
        let Some(next) = child(&base, name)? else {
            return Ok(0);
        };
        base = next;
    }
    let mut fanouts = BTreeMap::<(String, String), BTreeSet<&str>>::new();
    for hash in targets {
        if !is_hash(hash) {
            return Err(KioError::invalid_usage("invalid purge raw hash"));
        }
        let raw = &hash[7..];
        fanouts
            .entry((raw[..2].to_owned(), raw[2..4].to_owned()))
            .or_default()
            .insert(raw);
    }
    let mut inspected = 0usize;
    let mut removed = 0u64;
    for ((first, second), raw_targets) in fanouts {
        let Some(first) = child(&base, &first)? else {
            continue;
        };
        let Some(parent) = child(&first, &second)? else {
            continue;
        };
        let entries =
            cap_primitives::fs::read_base_dir(&parent.root_handle()).map_err(|error| {
                KioError::io(error.to_string(), parent.path().display().to_string())
            })?;
        let mut names = Vec::new();
        for entry in entries {
            inspected = inspected.saturating_add(1);
            if inspected > 100_000 {
                return Err(store_corrupt(
                    parent.path(),
                    "normalized cache inventory exceeds its bound",
                ));
            }
            let entry = entry.map_err(|error| {
                KioError::io(error.to_string(), parent.path().display().to_string())
            })?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                store_corrupt(parent.path(), "normalized cache name is not UTF-8")
            })?;
            let Some((raw, suffix)) = name.split_once('.') else {
                continue;
            };
            if !raw_targets.contains(raw) {
                continue;
            }
            let Some((tool, generation)) = suffix.split_once(".g") else {
                return Err(store_corrupt(
                    parent.path(),
                    "target normalized cache name is malformed",
                ));
            };
            if !is_hash(&format!("sha256:{tool}"))
                || generation
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|value| value.to_string() != generation)
            {
                return Err(store_corrupt(
                    parent.path(),
                    "target normalized cache identity is malformed",
                ));
            }
            // Opening and later deletion both use the retained parent. Links,
            // reparse points, hard-linked files and unsafe descendants fail
            // closed in StoreDirectory's recursive removal primitive.
            parent.open_directory(Path::new(name))?;
            names.push(name.to_owned());
        }
        names.sort();
        for name in names {
            parent.remove_directory_all(Path::new(&name))?;
            removed = removed.saturating_add(1);
            kio_core::durability::checkpoint(
                kio_core::durability::DurabilityPoint::PurgeCacheDeleted,
            )?;
        }
    }
    Ok(removed)
}

#[cfg(not(any(unix, windows)))]
fn remove_target_normalized_instances(
    _repo: &Repository,
    _targets: &BTreeSet<&str>,
) -> Result<u64> {
    Err(KioError::invalid_usage(
        "safe purge cache cleanup is unsupported on this platform",
    ))
}

fn scan_derived_inventory(
    store: &ObjectStore,
    kio_dir: &Path,
    targets: &BTreeSet<&str>,
) -> Result<DerivedInventory> {
    const MAX_INSTANCES: usize = 100_000;
    let root = kio_dir.join("objects/normalized_units");
    if !path_exists(&root)? {
        return Ok(DerivedInventory::default());
    }
    validate_real_directory(&root)?;
    let mut leaves = Vec::new();
    let mut first_count = 0usize;
    for first in read_real_directory(&root)? {
        first_count = first_count.saturating_add(1);
        if first_count > 256 {
            return Err(store_corrupt(&root, "normalized fanout exceeds its bound"));
        }
        validate_real_directory(&first)?;
        for second in read_real_directory(&first)? {
            validate_real_directory(&second)?;
            for leaf in read_real_directory(&second)? {
                if leaves.len() >= MAX_INSTANCES {
                    return Err(store_corrupt(
                        &root,
                        "normalized instance count exceeds its bound",
                    ));
                }
                validate_real_directory(&leaf)?;
                leaves.push(leaf);
            }
        }
    }

    let mut inventory = DerivedInventory::default();
    let mut identities = BTreeSet::new();
    for leaf in leaves {
        let manifest_path = leaf.join("manifest.json");
        let bytes = read_bounded_regular(&manifest_path, 8 * 1024 * 1024)?.ok_or_else(|| {
            store_corrupt(&manifest_path, "normalized instance manifest is missing")
        })?;
        let manifest: NormalizedInstanceManifest = serde_json::from_slice(&bytes)
            .map_err(|error| store_corrupt(&manifest_path, &error.to_string()))?;
        validate_instance_leaf(&leaf, &manifest)?;
        let identity = (
            manifest.raw_hash.clone(),
            manifest.tool_profile_hash.clone(),
            manifest.r#gen,
        );
        let target = targets.contains(manifest.raw_hash.as_str());
        // The mutable instance manifest merely locates the immutable manifest
        // CAS object. A purge may not derive a deletion closure from that
        // mutable copy alone: read the exact CAS bytes under a bound, verify
        // their digest, deserialize, and then validate every pinned Done unit
        // body before recording any unit hash for deletion.
        let manifest_hash = manifest_content_hash(&manifest)?;
        let manifest = read_verified_manifest_object(store, &manifest_hash, &leaf)?;
        validate_instance_leaf(&leaf, &manifest)?;
        let units = load_validated_normalized_units_from_manifest(store, &manifest)
            .map_err(crate::pipeline_to_kio)?;
        let unit_hashes = manifest
            .units
            .iter()
            .filter(|entry| entry.status == UnitStatus::Done)
            .map(|entry| {
                entry
                    .unit_object_hash
                    .clone()
                    .expect("validated done normalized unit hash")
            })
            .collect::<BTreeSet<_>>();
        if target {
            // PB04/item 3: the manifest CAS object hash this instance's tree
            // entries would carry as `normalize.manifest_hash` — computed
            // the same way indexing writes it (canonical JCS bytes of the
            // manifest struct), not read back from a tree entry (a purge
            // target's raw need not currently be bound in HEAD's tree at
            // all, e.g. a historical-only raw-hash purge).
            inventory.target_manifest_hashes.insert(manifest_hash);
            inventory.target_normalized_units.extend(unit_hashes);
        } else {
            inventory.surviving_normalized_units.extend(unit_hashes);
        }
        if !identities.insert(identity.clone()) {
            continue;
        }
        let (prepared, images) = normalized_references(&manifest, &units);
        if target {
            inventory.target_prepared.extend(prepared);
            inventory.target_images.extend(images);
        } else {
            inventory.surviving_prepared.extend(prepared);
            inventory.surviving_images.extend(images);
        }
    }
    Ok(inventory)
}

/// Add every immutable normalized manifest/unit closure that remains reachable
/// from *any* current ref. Mutable normalized-instance directories are useful
/// for deleting their cache projections, but they are not a history inventory:
/// a later same-generation retry can replace that directory while older tree
/// snapshots still pin a distinct manifest and immutable unit body.
#[derive(Debug)]
struct VerifiedManifestClosure {
    raw_hash: String,
    tool_profile_hash: String,
    r#gen: u64,
    unit_hashes: BTreeSet<String>,
    prepared: BTreeSet<String>,
    images: BTreeSet<String>,
}

fn extend_inventory_from_reachable_refs(
    repo: &Repository,
    targets: &BTreeSet<&str>,
    inventory: &mut DerivedInventory,
) -> Result<()> {
    let mut seen_manifest_hashes = BTreeSet::new();
    let mut verified_closures = BTreeMap::<String, VerifiedManifestClosure>::new();
    // Keep purge's liveness roots exactly in step with rebuild-db.  A durable
    // chunks.jsonl association can retain a disconnected commit after refs
    // moved away from it; its immutable manifest/unit/prepared/image closure
    // must therefore count as surviving content.  The shared helper treats
    // ledger values only as candidates: it authenticates creation and
    // publication introductions against immutable commit/tree state, allowing
    // only a missing pre-commit crash residue.  Do not re-add raw ledger
    // fields here by commit existence alone.
    let roots = crate::durable_history_roots(repo)?;
    if roots.is_empty() {
        return Ok(());
    }
    let graph = HistoryReader::new(repo.kio_dir()).walk_for_roots(&roots)?;
    for binding in graph.bindings() {
        let Some(normalize) = binding.binding.normalize.as_ref() else {
            continue;
        };
        let store = repo.object_store();
        let source_path = PathBuf::from("manifest CAS object");
        if !verified_closures.contains_key(&normalize.manifest_hash) {
            // A prior completed purge intentionally leaves the old tree/commit
            // for audit but removes its manifest closure. Once the raw has
            // been resurrected, that old absence is explainable only before
            // the recorded resurrection commit; the resurrection snapshot's
            // own manifest is still mandatory and any other absence fails
            // closed below.
            let manifest =
                match read_verified_manifest_object(&store, &normalize.manifest_hash, &source_path)
                {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        if historical_manifest_was_purged_before_resurrection(
                            repo,
                            &binding.binding.raw_hash,
                            &binding.commit_hash,
                        )? {
                            continue;
                        }
                        return Err(error);
                    }
                };
            let units = load_validated_normalized_units_from_manifest(&store, &manifest)
                .map_err(crate::pipeline_to_kio)?;
            let unit_hashes = manifest
                .units
                .iter()
                .filter(|entry| entry.status == UnitStatus::Done)
                .map(|entry| {
                    entry
                        .unit_object_hash
                        .clone()
                        .expect("validated done normalized unit hash")
                })
                .collect::<BTreeSet<_>>();
            let (prepared, images) = normalized_references(&manifest, &units);
            verified_closures.insert(
                normalize.manifest_hash.clone(),
                VerifiedManifestClosure {
                    raw_hash: manifest.raw_hash,
                    tool_profile_hash: manifest.tool_profile_hash,
                    r#gen: manifest.r#gen,
                    unit_hashes,
                    prepared,
                    images,
                },
            );
        }
        let closure = verified_closures
            .get(&normalize.manifest_hash)
            .expect("verified manifest closure was inserted");
        // Validate every tree binding before coalescing equal content hashes.
        // A forged binding for a different raw could otherwise reuse a target's
        // manifest hash and make purge classify the closure as target-only.
        // This tuple validation intentionally remains outside the cache lookup:
        // the CAS manifest/unit closure is read once, but every attestation is
        // still checked before inventory hash de-duplication.
        if closure.raw_hash != binding.binding.raw_hash
            || closure.tool_profile_hash != normalize.tool_profile_hash
            || closure.r#gen != normalize.r#gen
        {
            return Err(store_corrupt(
                &source_path,
                "manifest CAS object disagrees with its reachable tree binding",
            ));
        }
        if !seen_manifest_hashes.insert(normalize.manifest_hash.clone()) {
            continue;
        }
        if targets.contains(closure.raw_hash.as_str()) {
            inventory
                .target_manifest_hashes
                .insert(normalize.manifest_hash.clone());
            inventory
                .target_normalized_units
                .extend(closure.unit_hashes.iter().cloned());
            inventory
                .target_prepared
                .extend(closure.prepared.iter().cloned());
            inventory
                .target_images
                .extend(closure.images.iter().cloned());
        } else {
            inventory
                .surviving_normalized_units
                .extend(closure.unit_hashes.iter().cloned());
            inventory
                .surviving_prepared
                .extend(closure.prepared.iter().cloned());
            inventory
                .surviving_images
                .extend(closure.images.iter().cloned());
        }
    }
    Ok(())
}

fn historical_manifest_was_purged_before_resurrection(
    repo: &Repository,
    raw_hash: &str,
    binding_commit: &str,
) -> Result<bool> {
    let state = PurgeState::open(repo.kio_dir())?;
    let tombstone = state.read_tombstone(raw_hash)?;
    let receipt = state.read_erase_receipt(raw_hash)?;
    let final_event = canonical_final_event(
        tombstone.as_ref().map(|record| record.tail()),
        receipt.as_ref().map(|record| record.tail()),
    )?;
    let Some(final_event) = final_event else {
        return Ok(false);
    };
    if final_event.event.kind != EventKind::Retired {
        return Ok(false);
    }
    let Some(resurrection_commit) = final_event.event.resurrection_commit.as_deref() else {
        return Ok(false);
    };
    if resurrection_commit == binding_commit {
        return Ok(false);
    }
    Ok(HistoryReader::new(repo.kio_dir())
        .walk(resurrection_commit)?
        .node(binding_commit)
        .is_some())
}

const MAX_NORMALIZED_MANIFEST_OBJECT_BYTES: u64 = 8 * 1024 * 1024;

fn manifest_content_hash(manifest: &NormalizedInstanceManifest) -> Result<String> {
    let value =
        serde_json::to_value(manifest).map_err(|error| KioError::schema(error.to_string()))?;
    kio_core::cas::hash_json(&value)
}

/// Read the immutable manifest object which the mutable instance manifest
/// names. This is deliberately separate from the current-instance loader: a
/// purge must verify the exact manifest object *before* committing a durable
/// deletion closure, and must never infer unit-object liveness from a mutable
/// file after that point.
fn read_verified_manifest_object(
    store: &ObjectStore,
    manifest_hash: &str,
    source_path: &Path,
) -> Result<NormalizedInstanceManifest> {
    let bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            manifest_hash,
            MAX_NORMALIZED_MANIFEST_OBJECT_BYTES,
        )
        .map_err(|error| store_corrupt(source_path, &format!("manifest CAS object: {error}")))?;
    if hash_bytes(&bytes) != manifest_hash {
        return Err(store_corrupt(
            source_path,
            "manifest CAS object does not match its content hash",
        ));
    }
    let manifest: NormalizedInstanceManifest = serde_json::from_slice(&bytes)
        .map_err(|error| store_corrupt(source_path, &format!("manifest CAS object: {error}")))?;
    let canonical = canonical_json_bytes(
        &serde_json::to_value(&manifest)
            .map_err(|error| store_corrupt(source_path, &error.to_string()))?,
    )?;
    if canonical != bytes {
        return Err(store_corrupt(
            source_path,
            "manifest CAS object is not canonical JSON",
        ));
    }
    Ok(manifest)
}

fn validate_instance_leaf(path: &Path, manifest: &NormalizedInstanceManifest) -> Result<()> {
    if !is_hash(&manifest.raw_hash) || !is_hash(&manifest.tool_profile_hash) {
        return Err(store_corrupt(
            path,
            "normalized manifest contains an invalid hash",
        ));
    }
    let raw = manifest.raw_hash.trim_start_matches("sha256:");
    let tool = manifest.tool_profile_hash.trim_start_matches("sha256:");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| store_corrupt(path, "normalized instance leaf is not valid UTF-8"))?;
    let valid_name = name == format!("{raw}.{tool}.g{}", manifest.r#gen);
    let fanout = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let first = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    if !valid_name || first != Some(&raw[0..2]) || fanout != Some(&raw[2..4]) {
        return Err(store_corrupt(
            path,
            "normalized instance path disagrees with its manifest identity",
        ));
    }
    Ok(())
}

fn normalized_references(
    manifest: &NormalizedInstanceManifest,
    units: &[NormalizedUnitObject],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let prepared = manifest
        .units
        .iter()
        .map(|entry| entry.prepared_hash.clone())
        .collect::<BTreeSet<_>>();
    // Image liveness is an immutable adapter ownership assertion. Provider
    // metadata and Markdown may cite a foreign image or arbitrary hash, but
    // never establish a local CAS ownership edge. Conversely, ownership keeps
    // a persisted image live even when the Markdown does not cite it.
    let images = units
        .iter()
        .flat_map(|unit| unit.owned_image_hashes.iter().cloned())
        .collect::<BTreeSet<_>>();
    (prepared, images)
}

fn remove_target_normalized_views(kio_dir: &Path, targets: &BTreeSet<&str>) -> Result<u64> {
    let root = kio_dir.join("objects/normalized");
    if !path_exists(&root)? {
        return Ok(0);
    }
    validate_real_directory(&root)?;
    let mut candidates = Vec::new();
    for raw_hash in targets {
        let digest = raw_hash.trim_start_matches("sha256:");
        let fanout = root.join(&digest[0..2]).join(&digest[2..4]);
        if !path_exists(&fanout)? {
            continue;
        }
        validate_real_directory(&fanout)?;
        for path in read_real_directory(&fanout)? {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| store_corrupt(&path, "normalized view leaf is not valid UTF-8"))?;
            if name.starts_with(&format!("{digest}.")) && name.ends_with(".md") {
                validate_existing_regular(&path, Some(256 * 1024 * 1024))?;
                candidates.push(path);
            }
        }
    }
    let mut deleted = 0_u64;
    for path in candidates {
        fs::remove_file(&path)
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        deleted = deleted.saturating_add(1);
    }
    Ok(deleted)
}

/// PA40/41 (§L, U37): settle in-flight/orphaned-cleanup ledger reservations
/// for this purge's own scope+targets. Called from `execute_visible_phases`
/// strictly within the `prepared` phase — before the tombstone/erase-receipt
/// is published, physical deletion happens, or the purged commit is
/// published (PA40) — so `tasks.jsonl` is still fully intact when this runs
/// (source (a) below depends on that).
///
/// Two independently scope_id-gated sources, neither reconstructing the
/// ledger's 4-tuple key from CURRENT config (purge must not guess a provider
/// adapter_kind / tool_profile_hash from mutable configuration that may have
/// changed since the reservation was made):
///  (a) `tasks.jsonl` rows still present, looked up by `task.reservation_id`
///      (the `intent_token` persisted at charge time) — the original
///      mechanism, unchanged, just moved earlier.
///  (b) PA41(b): a direct `batch_requests` scan by `(scope_id, input_hash)`,
///      catching `intent_token` residue whose `tasks.jsonl` row is already
///      gone (source (a) alone cannot discover these — there is no task left
///      to iterate). Both in-flight (state 0/1, settled via
///      `recovery_settle_unknown`) and terminal-but-cleanup-pending (state
///      2/3 with `intent_token` still set, cleared via
///      `recovery_finish_cleanup` — no re-settlement, since an outcome was
///      already recorded) rows are handled, whichever applies.
/// `scope_id`-gating on (b) is what keeps a *different* scope's in-flight
/// request that happens to share the same raw_hash (`input_hash`) untouched
/// (PA41(a)) — (a) is inherently scope-safe already, since it is keyed by
/// THIS scope's own task's own `reservation_id` token value (globally
/// unique), never by raw_hash. `request_kind` (batch/sync) is not filtered on
/// — both apply uniformly (PA41(c)).
fn settle_inflight_reservations_for_purge(
    repo: &Repository,
    target_raw_hashes: &[String],
    report: &mut PurgeReport,
) -> Result<()> {
    let targets = target_raw_hashes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let scope_id = crate::scope_id(repo.kio_dir())?;
    let Some(ledger) = crate::open_optional_ledger_db()? else {
        // A fresh offline device has no billing state. A local reservation
        // without its operational ledger must be recovered before deleting it.
        if TaskStore::new(repo.kio_dir())
            .all()
            .map_err(crate::pipeline_to_kio)?
            .iter()
            .any(|task| task.reservation_id.is_some())
        {
            return Err(KioError::new(
                "KIO-E-LEDGER-UNINITIALIZED-001",
                "purge cannot discard reservation locators while the cost ledger is unavailable",
                json!({}),
                ExitCode::PermanentFailure,
            ));
        }
        return Ok(());
    };
    let mut settled = 0_u64;
    let mut handled = BTreeSet::<TaskKey>::new();

    // (a) tasks.jsonl-driven.
    let tasks_path = repo.kio_dir().join("tasks.jsonl");
    if path_exists(&tasks_path)? {
        validate_existing_regular(&tasks_path, Some(MAX_TASK_STORE_BYTES))?;
    }
    for task in TaskStore::new(repo.kio_dir())
        .all()
        .map_err(crate::pipeline_to_kio)?
    {
        let attributed = targets.contains(task.input_hash.as_str())
            || task
                .previous_raw_hash
                .as_deref()
                .is_some_and(|hash| targets.contains(hash));
        if !attributed {
            continue;
        }
        let Some(token) = task.reservation_id.as_deref() else {
            continue;
        };
        let resolution =
            resolve_abandon_selector(&ledger, &AbandonSelector::IntentToken(token.to_owned()))
                .map_err(crate::pipeline_to_kio)?;
        let AbandonResolution::Found(key) = resolution else {
            // Already terminal + cleaned (or the token never resolved) —
            // nothing left to settle, an idempotent no-op.
            continue;
        };
        if !handled.insert(key.clone()) {
            continue;
        }
        let Some(row) = get_batch_request(&ledger, &key).map_err(crate::pipeline_to_kio)? else {
            continue;
        };
        if row.state.is_inflight() {
            recovery_settle_unknown(&ledger, &key, token, row.estimated_usd, true)
                .map_err(crate::pipeline_to_kio)?;
            settled = settled.saturating_add(1);
        }
    }

    // (b) direct ledger scan for rows tasks.jsonl no longer references.
    let orphans = kio_pipeline::ledger::ops::requests_with_intent_for_scope(&ledger, &scope_id)
        .map_err(crate::pipeline_to_kio)?;
    for row in orphans {
        let TaskKey {
            adapter_kind,
            input_hash,
            tool_profile_hash,
            ..
        } = row.key;
        if !targets.contains(input_hash.as_str()) {
            continue;
        }
        let key = TaskKey::new(
            scope_id.clone(),
            adapter_kind,
            input_hash,
            tool_profile_hash,
        );
        if handled.contains(&key) {
            continue;
        }
        let Some(row) = get_batch_request(&ledger, &key).map_err(crate::pipeline_to_kio)? else {
            continue;
        };
        let Some(token) = row.intent_token.clone() else {
            continue;
        };
        if row.state.is_inflight() {
            recovery_settle_unknown(&ledger, &key, &token, row.estimated_usd, true)
                .map_err(crate::pipeline_to_kio)?;
            settled = settled.saturating_add(1);
        } else if row.cleanup_pending() {
            recovery_finish_cleanup(&ledger, &key, &token).map_err(crate::pipeline_to_kio)?;
        }
    }

    report.deleted.reservations = report.deleted.reservations.saturating_add(settled);
    Ok(())
}

fn delete_target_tasks(
    repo: &Repository,
    targets: &BTreeSet<&str>,
    chunk_ids: &BTreeSet<String>,
    report: &mut PurgeReport,
) -> Result<()> {
    let path = repo.kio_dir().join("tasks.jsonl");
    if path_exists(&path)? {
        validate_existing_regular(&path, Some(MAX_TASK_STORE_BYTES))?;
    }
    let store = TaskStore::new(repo.kio_dir());
    let tasks = store.all().map_err(crate::pipeline_to_kio)?;
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for task in tasks {
        let embedding_target = task.task_type == TaskType::Embedding
            && task
                .output_ref
                .strip_prefix("embedding:")
                .is_some_and(|chunk_id| chunk_ids.contains(chunk_id));
        if targets.contains(task.input_hash.as_str())
            || task
                .previous_raw_hash
                .as_deref()
                .is_some_and(|hash| targets.contains(hash))
            || embedding_target
        {
            removed.push(task);
        } else {
            kept.push(task);
        }
    }

    // Ledger reservation settlement already happened in the `prepared` phase
    // (`settle_inflight_reservations_for_purge`, PA40/41) — this is now pure
    // task-row removal.
    if !removed.is_empty() {
        store.replace_all(&kept).map_err(crate::pipeline_to_kio)?;
        report.deleted.tasks = report
            .deleted
            .tasks
            .saturating_add(u64::try_from(removed.len()).unwrap_or(u64::MAX));
    }
    Ok(())
}

fn scrub_manifest(kio_dir: &Path, targets: &BTreeSet<&str>) -> Result<u64> {
    let path = kio_dir.join("manifest.json");
    let Some(bytes) = read_bounded_regular(&path, 64 * 1024 * 1024)? else {
        return Ok(0);
    };
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|error| store_corrupt(&path, &error.to_string()))?;
    let files = value
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| store_corrupt(&path, "manifest files array is missing"))?;
    let before = files.len();
    files.retain(|row| {
        !row.get("raw_hash")
            .and_then(Value::as_str)
            .is_some_and(|hash| targets.contains(hash))
    });
    let removed = before.saturating_sub(files.len());
    if removed > 0 {
        let output = serde_json::to_vec_pretty(&value)
            .map_err(|error| KioError::schema(error.to_string()))?;
        atomic_private_replace(&path, &output)?;
    }
    Ok(u64::try_from(removed).unwrap_or(u64::MAX))
}

fn scrub_unsupported(kio_dir: &Path, targets: &BTreeSet<&str>) -> Result<u64> {
    let path = kio_dir.join("unsupported-inputs.jsonl");
    rewrite_jsonl_filter(&path, 16 * 1024 * 1024, |row| {
        row.get("raw_hash")
            .and_then(Value::as_str)
            .is_none_or(|hash| !targets.contains(hash))
    })
}

/// PA35 (§I, U32): staging descriptor attribution is enumerated by walking
/// `.kio/staging/` directly (05 §3.5 L718 — the durable-descriptor directory
/// itself, 03-data-model.md §2), **not** by reading `tasks.jsonl` — a task
/// whose record was already lost (compaction, prior failure) still leaves its
/// staging descriptor discoverable and deletable this way. Each descriptor is
/// a small JSON object; a descriptor attributed to one of `targets` (by its
/// `raw_hash` field) is removed regardless of the originating task's current
/// status (retryable-failed staging is included, same as any other). Missing
/// `.kio/staging/` (no producer has ever populated it in this scope) is a
/// vacuous zero, not an error — this walk is written against the directory's
/// *contract*, independent of whether any current call site produces it yet.
fn delete_target_staging(kio_dir: &Path, targets: &BTreeSet<&str>) -> Result<u64> {
    let root = kio_dir.join("staging");
    if !path_exists(&root)? {
        return Ok(0);
    }
    validate_real_directory(&root)?;
    let mut removed = 0_u64;
    for path in read_real_directory(&root)? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| store_corrupt(&path, "staging descriptor name is not valid UTF-8"))?;
        if !name.ends_with(".json") {
            continue;
        }
        let Some(bytes) = read_bounded_regular(&path, 8 * 1024 * 1024)? else {
            continue;
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            store_corrupt(&path, &format!("invalid staging descriptor: {error}"))
        })?;
        let attributed = value
            .get("raw_hash")
            .and_then(Value::as_str)
            .is_some_and(|hash| targets.contains(hash));
        if attributed {
            fs::remove_file(&path)
                .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

fn scrub_quarantine(kio_dir: &Path, historical_paths: &[String]) -> Result<u64> {
    let path = kio_dir.join("quarantine.jsonl");
    let paths = historical_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    rewrite_jsonl_filter(&path, 16 * 1024 * 1024, |row| {
        row.get("path")
            .and_then(Value::as_str)
            .is_none_or(|path| !paths.contains(path))
    })
}

fn rewrite_jsonl_filter(
    path: &Path,
    max_bytes: u64,
    mut retain: impl FnMut(&Value) -> bool,
) -> Result<u64> {
    let Some(bytes) = read_bounded_regular(path, max_bytes)? else {
        return Ok(0);
    };
    let mut kept = Vec::new();
    let mut removed = 0_u64;
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = serde_json::from_slice(line).map_err(|error| {
            store_corrupt(path, &format!("invalid JSONL row {}: {error}", index + 1))
        })?;
        if retain(&value) {
            serde_json::to_writer(&mut kept, &value)
                .map_err(|error| KioError::schema(error.to_string()))?;
            kept.push(b'\n');
        } else {
            removed = removed.saturating_add(1);
        }
    }
    if removed > 0 {
        atomic_private_replace(path, &kept)?;
    }
    Ok(removed)
}

/// PA03/PA11-13: `open_cache_path` (main.rs) nests `image` hashes under an
/// extra `image/` type segment so a same-digest raw/image pair never shares
/// one cache directory; eviction must target the same namespace or it either
/// misses an image's cache dir entirely or (worse) deletes a same-digest
/// raw/prepared cache dir that a live, non-target image object still needs.
fn evict_open_cache(hashes: &BTreeSet<String>, is_image: bool) -> Result<u64> {
    let mut root = crate::cache_home().join("kio/open");
    if is_image {
        root = root.join("image");
    }
    if !path_exists(&root)? {
        return Ok(0);
    }
    validate_real_directory(&root)?;
    let mut removed = 0_u64;
    for hash in hashes {
        let path = root.join(hash.trim_start_matches("sha256:"));
        if remove_tree_nofollow(&path)? {
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

fn scrub_logs(
    repo: &Repository,
    plan: &PurgePlan,
    journal: &PurgeJournal,
    report: &mut PurgeReport,
) -> Result<()> {
    let device_root = crate::data_home().join("kio/logs");
    let scope_root = repo.kio_dir().join("logs");
    let _device_lock = StoreLock::acquire_path(device_root.join("scrub.lock"))?;
    let _scope_lock = StoreLock::acquire_path(scope_root.join("access.scrub.lock"))?;

    let identifiers = plan
        .target_raw_hashes
        .iter()
        .chain(plan.historical_paths.iter())
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    // PA36 (§J, U33): `events`/`errors`/`metrics` are device-global (shared by
    // every scope on the device) — a row is only in scope for THIS purge if
    // its own `scope_id` field equals this scope's (a row with no `scope_id`
    // field at all — not yet retrofitted to carry one — stays eligible, the
    // conservative pre-PA36 behavior, rather than silently becoming
    // permanently unscrubbable). `access` logs live under this scope's own
    // `.kio/logs/`, already scoped by directory location, so no additional
    // gate is needed there.
    let scope_id = crate::scope_id(repo.kio_dir())?;
    let mut pass_files = 0_u64;
    let mut pass_rows = 0_u64;
    let mut pass_fields = 0_u64;
    for path in collect_log_files(&device_root, &["events", "errors", "metrics"])? {
        let (rows, fields) = scrub_one_log(&path, &identifiers, Some(scope_id.as_str()))?;
        pass_files = pass_files.saturating_add(1);
        pass_rows = pass_rows.saturating_add(rows);
        pass_fields = pass_fields.saturating_add(fields);
    }
    for path in collect_log_files(&scope_root, &["access"])? {
        let (rows, fields) = scrub_one_log(&path, &identifiers, None)?;
        pass_files = pass_files.saturating_add(1);
        pass_rows = pass_rows.saturating_add(rows);
        pass_fields = pass_fields.saturating_add(fields);
    }
    report.log_files_scrubbed = report.log_files_scrubbed.max(pass_files);
    report.log_rows_removed = report.log_rows_removed.saturating_add(pass_rows);
    report.log_fields_masked = report.log_fields_masked.saturating_add(pass_fields);

    if journal.phase == PurgePhase::Tombstoned {
        append_event_log(
            "KIO-PURGE-COMPLETED",
            "purge removed content from KIO-managed history",
            json!({
                "actor": journal.actor,
                "reason": plan.reason,
                "purged_in_commit": journal.planned_commit,
                "target_raw_count": plan.target_raw_hashes.len(),
                "deleted_counts": report.deleted,
            }),
        )?;
    }
    Ok(())
}

fn finalize_purge(
    repo: &Repository,
    state: &PurgeState,
    plan: &PurgePlan,
    journal: &PurgeJournal,
    report: &mut PurgeReport,
) -> Result<()> {
    // Hold both append/scrub locks across the final pass and journal removal.
    // `scrub_logs` re-acquires these paths reentrantly, so its inner guards drop
    // without releasing the outer exclusion window.
    let device_root = crate::data_home().join("kio/logs");
    let scope_root = repo.kio_dir().join("logs");
    let _device_lock = StoreLock::acquire_path(device_root.join("scrub.lock"))?;
    let _scope_lock = StoreLock::acquire_path(scope_root.join("access.scrub.lock"))?;
    scrub_logs(repo, plan, journal, report)?;
    remove_purge_sidecars(repo.kio_dir())?;
    state.finish(journal)
}

fn collect_log_files(root: &Path, prefixes: &[&str]) -> Result<Vec<PathBuf>> {
    if !path_exists(root)? {
        return Ok(Vec::new());
    }
    validate_real_directory(root)?;
    let mut output = Vec::new();
    for path in read_real_directory(root)? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| store_corrupt(&path, "log file name is not valid UTF-8"))?;
        if name.ends_with(".lock") || !prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        validate_existing_regular(&path, Some(64 * 1024 * 1024))?;
        output.push(path);
    }
    Ok(output)
}

/// `scope_gate`: `Some(scope_id)` restricts deletion/masking to rows whose own
/// `scope_id` field equals it (a row with no `scope_id` field at all stays
/// eligible — PA36's device-global gate; see `scrub_logs`). `None` disables
/// gating (used for scope-local log directories, already scoped by path).
fn scrub_one_log(
    path: &Path,
    identifiers: &BTreeSet<&str>,
    scope_gate: Option<&str>,
) -> Result<(u64, u64)> {
    let Some(bytes) = read_bounded_regular(path, 64 * 1024 * 1024)? else {
        return Ok((0, 0));
    };
    let mut output = Vec::new();
    let mut removed = 0_u64;
    let mut masked = 0_u64;
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut row: Value = serde_json::from_slice(line).map_err(|error| {
            store_corrupt(path, &format!("invalid log row {}: {error}", index + 1))
        })?;
        let in_scope = scope_gate.is_none_or(|scope_id| {
            row.get("scope_id")
                .and_then(Value::as_str)
                .is_none_or(|row_scope_id| row_scope_id == scope_id)
        });
        if in_scope && value_contains_identifier(&row, identifiers) {
            removed = removed.saturating_add(1);
            continue;
        }
        if in_scope {
            mask_sensitive_log_fields(&mut row, &mut masked);
        }
        serde_json::to_writer(&mut output, &row)
            .map_err(|error| KioError::schema(error.to_string()))?;
        output.push(b'\n');
    }
    if removed > 0 || masked > 0 {
        atomic_private_replace(path, &output)?;
    }
    Ok((removed, masked))
}

fn value_contains_identifier(value: &Value, identifiers: &BTreeSet<&str>) -> bool {
    match value {
        Value::String(text) => identifiers
            .iter()
            .any(|identifier| !identifier.is_empty() && text.contains(identifier)),
        Value::Array(array) => array
            .iter()
            .any(|child| value_contains_identifier(child, identifiers)),
        Value::Object(object) => object
            .values()
            .any(|child| value_contains_identifier(child, identifiers)),
        _ => false,
    }
}

fn mask_sensitive_log_fields(value: &mut Value, count: &mut u64) {
    match value {
        Value::Array(array) => {
            for child in array {
                mask_sensitive_log_fields(child, count);
            }
        }
        Value::Object(object) => {
            for (key, child) in object {
                if matches!(key.as_str(), "query" | "prompt") && !child.is_null() {
                    *child = json!("[redacted-by-purge]");
                    *count = count.saturating_add(1);
                } else {
                    mask_sensitive_log_fields(child, count);
                }
            }
        }
        _ => {}
    }
}

fn remove_purge_sidecars(kio_dir: &Path) -> Result<()> {
    for path in [phase_chunk_ids_path(kio_dir), phase_report_path(kio_dir)] {
        if path_exists(&path)? {
            validate_existing_regular(&path, Some(8 * 1024 * 1024))?;
            fs::remove_file(&path)
                .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        }
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(KioError::io(error.to_string(), path.display().to_string())),
    }
}

fn validate_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(store_corrupt(path, "expected a real directory"));
    }
    // On Windows, `is_symlink` does not cover every directory reparse-point
    // kind (notably junctions).  Purge recursively deletes normalized/cache
    // trees, so descending through one could unlink files outside Kio.  Verify
    // the opened directory handle before every traversal step.
    #[cfg(windows)]
    if !kio_core::cas::windows_directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(store_corrupt(path, "expected a real non-reparse directory"));
    }
    Ok(())
}

fn read_real_directory(path: &Path) -> Result<Vec<PathBuf>> {
    validate_real_directory(path)?;
    let mut output = Vec::new();
    let entries = fs::read_dir(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        if output.len() >= 100_000 {
            return Err(store_corrupt(
                path,
                "directory entry count exceeds its bound",
            ));
        }
        output.push(entry.path());
    }
    output.sort();
    Ok(output)
}

fn validate_existing_regular(path: &Path, max_bytes: Option<u64>) -> Result<fs::Metadata> {
    let listed = fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if listed.file_type().is_symlink() || !listed.file_type().is_file() {
        return Err(store_corrupt(path, "expected a real regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if listed.nlink() != 1 {
            return Err(store_corrupt(
                path,
                "regular file has an unexpected hard-link count",
            ));
        }
    }
    #[cfg(windows)]
    let _ = windows_regular_file_identity(path)?;
    if max_bytes.is_some_and(|limit| listed.len() > limit) {
        return Err(store_corrupt(path, "regular file exceeds its byte bound"));
    }
    Ok(listed)
}

#[cfg(windows)]
fn windows_regular_file_identity(path: &Path) -> Result<kio_core::cas::WindowsRegularFileIdentity> {
    kio_core::cas::windows_real_regular_file_identity(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        .ok_or_else(|| store_corrupt(path, "expected a real single-link regular file"))
}

fn read_bounded_regular(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    if !path_exists(path)? {
        return Ok(None);
    }
    let listed = validate_existing_regular(path, Some(max_bytes))?;
    #[cfg(windows)]
    let listed_identity = windows_regular_file_identity(path)?;
    let mut file = kio_core::cas::open_regular_nofollow(path)
        .map_err(|_| store_corrupt(path, "regular file changed while it was opened"))?;
    let opened = file
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    #[cfg(windows)]
    let identity_matches = same_file_identity(&listed, &opened)
        && kio_core::cas::windows_regular_file_handle_identity(&file) == Some(listed_identity);
    #[cfg(not(windows))]
    let identity_matches = same_file_identity(&listed, &opened);
    if !identity_matches {
        return Err(store_corrupt(
            path,
            "regular file changed while it was opened",
        ));
    }
    let capacity = usize::try_from(opened.len().min(max_bytes)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(store_corrupt(path, "regular file exceeds its byte bound"));
    }
    Ok(Some(bytes))
}

fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len()
            && left.modified().ok() == right.modified().ok()
            && left.file_type() == right.file_type()
    }
}

struct TreeRemoval {
    path: PathBuf,
    expected: fs::Metadata,
    is_directory: bool,
    #[cfg(windows)]
    directory_identity: Option<kio_core::cas::WindowsDirectoryIdentity>,
    #[cfg(windows)]
    regular_file_identity: Option<kio_core::cas::WindowsRegularFileIdentity>,
}

fn remove_tree_nofollow(path: &Path) -> Result<bool> {
    if !path_exists(path)? {
        return Ok(false);
    }
    validate_real_directory(path)?;
    let mut removals = Vec::<TreeRemoval>::new();
    collect_tree_removals(path, &mut removals, 0)?;
    for removal in removals {
        let entry = removal.path;
        let current = fs::symlink_metadata(&entry)
            .map_err(|error| KioError::io(error.to_string(), entry.display().to_string()))?;
        #[cfg(windows)]
        let identity_matches = if removal.is_directory {
            kio_core::cas::windows_real_directory_identity(&entry)
                .map_err(|error| KioError::io(error.to_string(), entry.display().to_string()))?
                == removal.directory_identity
        } else {
            same_file_identity(&removal.expected, &current)
                && Some(windows_regular_file_identity(&entry)?) == removal.regular_file_identity
        };
        #[cfg(not(windows))]
        let identity_matches = same_file_identity(&removal.expected, &current);
        if !identity_matches {
            return Err(store_corrupt(&entry, "artifact changed before deletion"));
        }
        if removal.is_directory {
            fs::remove_dir(&entry)
                .map_err(|error| KioError::io(error.to_string(), entry.display().to_string()))?;
        } else {
            fs::remove_file(&entry)
                .map_err(|error| KioError::io(error.to_string(), entry.display().to_string()))?;
        }
    }
    Ok(true)
}

fn collect_tree_removals(path: &Path, output: &mut Vec<TreeRemoval>, depth: usize) -> Result<()> {
    if depth > 16 || output.len() >= 100_000 {
        return Err(store_corrupt(
            path,
            "artifact tree exceeds its deletion bound",
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if metadata.file_type().is_symlink() {
        return Err(store_corrupt(
            path,
            "artifact tree contains a symbolic link",
        ));
    }
    if metadata.is_dir() {
        #[cfg(windows)]
        let directory_identity = kio_core::cas::windows_real_directory_identity(path)
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
            .ok_or_else(|| store_corrupt(path, "artifact tree contains a reparse directory"))?;
        for child in read_real_directory(path)? {
            collect_tree_removals(&child, output, depth + 1)?;
        }
        output.push(TreeRemoval {
            path: path.to_path_buf(),
            expected: metadata,
            is_directory: true,
            #[cfg(windows)]
            directory_identity: Some(directory_identity),
            #[cfg(windows)]
            regular_file_identity: None,
        });
    } else if metadata.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(store_corrupt(
                    path,
                    "artifact has an unexpected hard-link count",
                ));
            }
        }
        #[cfg(windows)]
        let regular_file_identity = windows_regular_file_identity(path)?;
        output.push(TreeRemoval {
            path: path.to_path_buf(),
            expected: metadata,
            is_directory: false,
            #[cfg(windows)]
            directory_identity: None,
            #[cfg(windows)]
            regular_file_identity: Some(regular_file_identity),
        });
    } else {
        return Err(store_corrupt(
            path,
            "artifact tree contains a non-regular entry",
        ));
    }
    Ok(())
}

fn store_corrupt(path: &Path, message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "purge encountered corrupt store state",
        json!({ "path": path.display().to_string(), "message": message }),
        ExitCode::PermanentFailure,
    )
}

/// PA37 (§K, U34; §R ruling #1): the warning text both the interactive
/// preview (stderr) and the structured report (`working_tree_warning` field)
/// carry — "will be re-ingested on the next `kio index`" plus the two ways to
/// permanently exclude it.
fn working_tree_warning_text(alias_count: u64) -> String {
    format!(
        "warning: {alias_count} working-tree file(s) still contain the exact bytes being \
         purged. The next `kio index` will re-discover and re-ingest them, making their \
         pointer alive again. To permanently exclude them, delete the file(s) or add them to \
         `.kioignore`."
    )
}

fn attach_working_tree_warning(mut value: Value, alias_count: u64) -> Value {
    if alias_count > 0
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "working_tree_warning".to_owned(),
            json!({
                "live_alias_count": alias_count,
                "message": working_tree_warning_text(alias_count),
            }),
        );
    }
    value
}

fn completed_report(plan: &PurgePlan, completed: CompletedTerminal) -> Value {
    let report = PurgeReport {
        purged_in_commit: Some(completed.purged_in_commit),
        tombstone_count: completed.tombstone_count,
        erase_receipt_count: completed.erase_receipt_count,
        ..PurgeReport::default()
    };
    success_report(plan, &report)
}

fn success_report(plan: &PurgePlan, report: &PurgeReport) -> Value {
    json!({
        "status": "purged",
        "purged_in_commit": report.purged_in_commit,
        "reason": plan.reason,
        "target_raw_count": plan.target_raw_hashes.len(),
        "deleted_counts": report.deleted,
        "shared_artifacts_preserved": report.shared,
        "tombstone_mode": plan.tombstone_mode,
        "tombstone_count": report.tombstone_count,
        "erase_receipt_count": report.erase_receipt_count,
        "logs_scrubbed": true,
        "log_files_scrubbed": report.log_files_scrubbed,
        "log_rows_removed": report.log_rows_removed,
        "log_fields_masked": report.log_fields_masked,
        "guarantee": "removed from KIO-managed history",
        "not_covered": [
            "external backups and Time Machine",
            "exported or manually copied files",
            "cloud-sync past versions",
            "logs outside Kio management"
        ],
    })
}

fn incomplete_report(
    plan: &PurgePlan,
    journal: &PurgeJournal,
    report: &PurgeReport,
    error: &KioError,
) -> Value {
    let mut value = success_report(plan, report);
    value["status"] = json!("purge_incomplete");
    value["logs_scrubbed"] = json!(journal.phase >= PurgePhase::Deleted);
    value["error_code"] = json!("KIO-E-PURGE-INCOMPLETE-001");
    value["message"] = json!("purge is incomplete and will resume on the next identical command");
    value["failure_phase"] = json!(journal.phase);
    value["cause_error_code"] = json!(error.error_code());
    crate::set_exit_override(&mut value, ExitCode::PartialFailure);
    value
}

fn maybe_inject_fault(phase: &str) -> Result<()> {
    if purge_fault_enabled(phase) {
        return Err(KioError::io(
            format!("injected purge failure after {phase}"),
            "purge fault seam",
        ));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn purge_fault_enabled(phase: &str) -> bool {
    let expected = match phase {
        "prepared" => PurgeFault::Prepared,
        "prepared_visible" => PurgeFault::PreparedVisible,
        "tombstoned" => PurgeFault::Tombstoned,
        "deleted" => PurgeFault::Deleted,
        "committed" => PurgeFault::Committed,
        _ => return false,
    };
    crate::debug_test_control()
        .expect("command dispatch installs debug test control")
        .cli
        .purge_fail_after_phase
        .known()
        == Some(&expected)
}

#[cfg(not(debug_assertions))]
fn purge_fault_enabled(_: &str) -> bool {
    false
}

fn atomic_private_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| {
        KioError::io("purge state path has no parent", path.display().to_string())
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| KioError::io(error.to_string(), parent.display().to_string()))?;
    let metadata = fs::symlink_metadata(parent)
        .map_err(|error| KioError::io(error.to_string(), parent.display().to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KioError::io(
            "purge state parent is not a real directory",
            parent.display().to_string(),
        ));
    }
    #[cfg(windows)]
    let parent_identity = kio_core::cas::windows_real_directory_identity(parent)
        .map_err(|error| KioError::io(error.to_string(), parent.display().to_string()))?
        .ok_or_else(|| store_corrupt(parent, "purge state parent is a reparse directory"))?;
    let original_leaf = if path_exists(path)? {
        Some(validate_existing_regular(path, None)?)
    } else {
        None
    };
    #[cfg(windows)]
    let original_leaf_identity = original_leaf
        .as_ref()
        .map(|_| windows_regular_file_identity(path))
        .transpose()?;
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".purge-write-{}-{nanos}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|error| KioError::io(error.to_string(), temp.display().to_string()))?;
        file.write_all(bytes)
            .map_err(|error| KioError::io(error.to_string(), temp.display().to_string()))?;
        file.sync_all()
            .map_err(|error| KioError::io(error.to_string(), temp.display().to_string()))?;
        drop(file);
        #[cfg(windows)]
        {
            let current_parent = kio_core::cas::windows_real_directory_identity(parent)
                .map_err(|error| KioError::io(error.to_string(), parent.display().to_string()))?;
            if current_parent != Some(parent_identity) {
                return Err(store_corrupt(
                    parent,
                    "purge state parent changed during rewrite",
                ));
            }
        }
        #[cfg(not(windows))]
        {
            let current_parent = fs::symlink_metadata(parent)
                .map_err(|error| KioError::io(error.to_string(), parent.display().to_string()))?;
            if !same_file_identity(&metadata, &current_parent) {
                return Err(store_corrupt(
                    parent,
                    "purge state parent changed during rewrite",
                ));
            }
        }
        match &original_leaf {
            Some(expected) => {
                let current = validate_existing_regular(path, None)?;
                #[cfg(windows)]
                let identity_matches = same_file_identity(expected, &current)
                    && Some(windows_regular_file_identity(path)?) == original_leaf_identity;
                #[cfg(not(windows))]
                let identity_matches = same_file_identity(expected, &current);
                if !identity_matches {
                    return Err(store_corrupt(
                        path,
                        "purge state file changed during rewrite",
                    ));
                }
            }
            None if path_exists(path)? => {
                return Err(store_corrupt(
                    path,
                    "purge state leaf appeared during rewrite",
                ));
            }
            None => {}
        }
        fs::rename(&temp, path)
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        if let Ok(directory) = fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn preflight(repo: &Repository, args: &PurgeArgs) -> Result<PurgePreview> {
    let plan = resolve_plan(repo, args)?;
    let state = PurgeState::open(repo.kio_dir())?;
    let journal = state.read_journal()?;
    if let Some(active) = &journal
        && (active.target_raw_hashes != plan.target_raw_hashes
            || active.reason != plan.reason
            || active.tombstone_mode != plan.tombstone_mode)
    {
        return Err(KioError::new(
            "KIO-E-PURGE-INCOMPLETE-001",
            "a different purge transaction is already in progress",
            json!({
                "phase": active.phase,
                "target_raw_count": active.target_raw_hashes.len(),
            }),
            ExitCode::PartialFailure,
        ));
    }

    let completed = if journal.is_none() {
        inspect_terminal_state(&state, &plan)?
    } else {
        inspect_terminal_state(&state, &plan)?;
        None
    };
    verify_targets_exist(repo, &plan, journal.is_some(), completed.is_some())?;
    // PA37 (§K, U34; §R ruling #1): warning-only, computed for the preview
    // display — never blocks the preview itself.
    let working_tree_alias_count = detect_live_working_copy(repo, &plan.target_raw_hashes)?;

    Ok(PurgePreview {
        plan,
        completed,
        working_tree_alias_count,
    })
}

fn resolve_plan(repo: &Repository, args: &PurgeArgs) -> Result<PurgePlan> {
    let reason = PurgeReason::from_str(&args.reason)?;
    let tombstone_mode = if args.erase_tombstone {
        TombstoneMode::Erase
    } else {
        TombstoneMode::Default
    };
    let head_commit = repo
        .head_commit_hash()?
        .ok_or_else(|| KioError::invalid_usage("cannot purge an unborn scope"))?;

    let (target_raw_hashes, historical_paths) = match (&args.path, &args.raw_hash) {
        (Some(path), None) => {
            let logical_path = path
                .to_str()
                .ok_or_else(|| KioError::invalid_usage("purge path must be valid UTF-8"))?;
            // Reuse the persisted TreeEntry boundary rather than interpreting a
            // deleted historical operand through host filesystem canonicalization.
            TreeEntry::raw_file(logical_path, VALIDATION_HASH)?;
            let roots = crate::durable_history_roots(repo)?;
            let bindings = if roots.is_empty() {
                Vec::new()
            } else {
                HistoryReader::new(repo.kio_dir())
                    .walk_for_roots(&roots)?
                    .bindings()
            };
            let targets = bindings
                .iter()
                .filter(|binding| binding.binding.path == logical_path)
                .map(|binding| binding.binding.raw_hash.clone())
                .collect::<BTreeSet<_>>();
            if targets.is_empty() {
                return Err(purge_not_found());
            }
            let paths = bindings
                .iter()
                .filter(|binding| targets.contains(&binding.binding.raw_hash))
                .map(|binding| binding.binding.path.clone())
                .collect::<BTreeSet<_>>();
            (targets.into_iter().collect(), paths.into_iter().collect())
        }
        (None, Some(raw_hash)) => {
            if !is_hash(raw_hash) {
                return Err(KioError::invalid_usage(
                    "--raw-hash must be sha256 followed by 64 lowercase hexadecimal digits",
                ));
            }
            // Raw identity remains authoritative even when history is shallow or
            // beyond traversal bounds. A successful best-effort walk only
            // enriches the conservative log/quarantine alias scrub.
            let paths = HistoryReader::new(repo.kio_dir())
                .walk(&head_commit)
                .ok()
                .into_iter()
                .flat_map(|graph| graph.bindings())
                .filter(|binding| binding.binding.raw_hash == *raw_hash)
                .map(|binding| binding.binding.path)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            (vec![raw_hash.clone()], paths)
        }
        _ => {
            return Err(KioError::invalid_usage(
                "purge requires exactly one of a path or --raw-hash",
            ));
        }
    };

    Ok(PurgePlan {
        head_commit,
        target_raw_hashes,
        historical_paths,
        reason,
        tombstone_mode,
    })
}

/// LC58-LC60: presence of a marker no longer means "dead" — only an *active*
/// tail (LC1/LC2) does. A retired marker is not terminal state; re-running
/// this exact purge against it starts a fresh purge (its own new `purged`/
/// `erased` event). M-ruling #2 (LC60): re-purging an active tombstone/receipt
/// has no reason-match requirement (dropped from the pre-Step4b flat-schema
/// rejection).
fn inspect_terminal_state(
    state: &PurgeState,
    plan: &PurgePlan,
) -> Result<Option<CompletedTerminal>> {
    let mut tombstones = Vec::new();
    let mut receipts = Vec::new();
    for raw_hash in &plan.target_raw_hashes {
        if let Some(tombstone) = state.read_tombstone(raw_hash)?
            && tombstone.is_active()
        {
            if plan.tombstone_mode == TombstoneMode::Erase {
                return Err(KioError::invalid_usage(
                    "converting an existing active tombstone to erase mode is not supported",
                ));
            }
            tombstones.push(tombstone);
        }
        if let Some(receipt) = state.read_erase_receipt(raw_hash)?
            && receipt.is_active()
        {
            receipts.push(receipt);
        }
    }

    let target_count = plan.target_raw_hashes.len();
    let completed = match plan.tombstone_mode {
        TombstoneMode::Default if tombstones.len() == target_count => Some(CompletedTerminal {
            purged_in_commit: tombstones[0].tail().in_commit.clone(),
            tombstone_count: u64::try_from(tombstones.len()).unwrap_or(u64::MAX),
            erase_receipt_count: 0,
        }),
        TombstoneMode::Erase if receipts.len() == target_count => Some(CompletedTerminal {
            purged_in_commit: receipts[0].tail().in_commit.clone(),
            tombstone_count: 0,
            erase_receipt_count: u64::try_from(receipts.len()).unwrap_or(u64::MAX),
        }),
        _ => None,
    };

    if completed.is_none() && (!tombstones.is_empty() || !receipts.is_empty()) {
        return Err(KioError::new(
            "KIO-E-PURGE-INCOMPLETE-001",
            "only part of the requested purge target has terminal state",
            json!({ "target_raw_count": target_count }),
            ExitCode::PartialFailure,
        ));
    }
    Ok(completed)
}

fn verify_targets_exist(
    repo: &Repository,
    plan: &PurgePlan,
    has_matching_journal: bool,
    is_complete: bool,
) -> Result<()> {
    if has_matching_journal || is_complete {
        return Ok(());
    }
    let store = ObjectStore::new(repo.kio_dir());
    for raw_hash in &plan.target_raw_hashes {
        match store.inspect_object(ObjectKind::Raw, raw_hash) {
            Ok(_) => {}
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                return Err(purge_not_found());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// PA37-39 (§K, U34; §R ruling #1): count working-tree entries whose bytes
/// match a purge target, by raw_hash content identity — matches regardless of
/// the entry's current path/name (ruling #1: no same-path-vs-renamed-alias
/// distinction; both are the same "same bytes residual" case). Purely
/// informational: the caller attaches this as a warning, never as a block —
/// `KIO-E-PURGE-WORKING-COPY-001` (the prior hard block) is retired.
fn detect_live_working_copy(repo: &Repository, targets: &[String]) -> Result<u64> {
    let targets = targets.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let working_tree = repo.build_working_tree(false)?;
    let live_alias_count = working_tree
        .tree
        .entries
        .iter()
        .filter(|entry| targets.contains(entry.raw_hash.as_str()))
        .count();
    Ok(u64::try_from(live_alias_count).unwrap_or(u64::MAX))
}

fn purge_not_found() -> KioError {
    KioError::new(
        "KIO-E-PURGE-NOT-FOUND-001",
        "purge target was not found in this scope",
        json!({}),
        ExitCode::PermanentFailure,
    )
}

fn confirm(preview: &PurgePreview, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }

    let mode = match preview.plan.tombstone_mode {
        TombstoneMode::Default => "default tombstone",
        TombstoneMode::Erase => "fsck-only erase receipt",
    };
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "Purge preview: {} raw object(s); reason={}; terminal={mode}.",
        preview.plan.target_raw_hashes.len(),
        preview.plan.reason
    )
    .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    if preview.completed.is_some() {
        writeln!(stderr, "This exact purge is already complete.")
            .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    }
    if preview.working_tree_alias_count > 0 {
        writeln!(
            stderr,
            "{}",
            working_tree_warning_text(preview.working_tree_alias_count)
        )
        .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    }
    write!(
        stderr,
        "Remove this content from KIO-managed history? [y/N] "
    )
    .map_err(|error| KioError::io(error.to_string(), "stderr"))?;
    stderr
        .flush()
        .map_err(|error| KioError::io(error.to_string(), "stderr"))?;

    // Bound confirmation input so a hostile pipe cannot force an unbounded
    // allocation. Inputs longer than 32 bytes simply fail the exact y/yes check.
    if crate::context::confirm("Remove this content from KIO-managed history? [y/N]")? {
        return Ok(());
    }

    Err(KioError::new(
        "KIO-E-PURGE-CONFIRMATION-REJECTED-001",
        "purge confirmation was rejected",
        json!({
            "target_raw_count": preview.plan.target_raw_hashes.len(),
        }),
        ExitCode::ConfirmationRejected,
    ))
}

#[cfg(test)]
mod image_ownership_tests {
    use super::*;
    use kio_pipeline::markdownize::MarkdownizeMode;
    use kio_pipeline::prepare::UnitType;

    fn hash(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn normalized_image_liveness_uses_ownership_not_provider_references() {
        let owned = hash('a');
        let provider_claim = hash('b');
        let unit = NormalizedUnitObject {
            unit_key: "page:1".to_owned(),
            unit_type: UnitType::Page,
            raw_hash: hash('c'),
            prepared_hash: hash('d'),
            preparation_profile_hash: hash('e'),
            tool_profile_hash: hash('f'),
            r#gen: 0,
            mode: MarkdownizeMode::Full,
            markdown: format!("![foreign](kio://foreign/object/image/{provider_claim})"),
            owned_image_hashes: BTreeSet::from([owned.clone()]),
            metadata: BTreeMap::from([("image_hash".to_owned(), json!(provider_claim))]),
            reused_from: None,
            generated_at: "2026-09-08T00:00:00Z".to_owned(),
        };
        let manifest = NormalizedInstanceManifest {
            raw_hash: unit.raw_hash.clone(),
            tool_profile_hash: unit.tool_profile_hash.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "owned-image-test".to_owned(),
            units: Vec::new(),
            generated_at: unit.generated_at.clone(),
        };
        let (_prepared, images) = normalized_references(&manifest, &[unit]);
        assert_eq!(images, BTreeSet::from([owned]));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::process::Command;

    use super::{atomic_private_replace, remove_tree_nofollow, validate_real_directory};

    #[test]
    fn ct4_atomic_private_replace_uses_stable_directory_identity() {
        let root = tempfile::tempdir().unwrap();
        let purge_dir = root.path().join("purge");
        let path = purge_dir.join("chunk-ids.json");

        atomic_private_replace(&path, b"[]").unwrap();
        atomic_private_replace(&path, b"[\"replacement\"]").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"[\"replacement\"]");
        assert!(std::fs::read_dir(&purge_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".purge-write-")
        }));
    }

    #[test]
    fn ct4_remove_tree_uses_stable_directory_identity_after_child_removal() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("normalized");
        let nested = tree.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("artifact.md"), b"content").unwrap();

        assert!(remove_tree_nofollow(&tree).unwrap());
        assert!(!tree.exists());
    }

    #[test]
    fn ct4_purge_rejects_directory_junction_before_traversal() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let junction = root.path().join("poisoned-junction");
        let status = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .status()
            .expect("cmd.exe must be available on Windows");
        assert!(status.success(), "test junction creation failed");

        let error = validate_real_directory(&junction).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        std::fs::remove_dir(&junction).unwrap();
    }
}
