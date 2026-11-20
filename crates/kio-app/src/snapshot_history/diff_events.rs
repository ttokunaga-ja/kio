use crate::historical_reindex::visit_pinned_done_units;
use crate::index::chunk_ledger::{
    ChunkPublicationEvent, StoredChunk, chunks_jsonl_path, sqlite_path, visit_chunk_ledger,
};
use crate::index::rebuild::{
    PublicationAuthenticationCache, authenticate_publication_event_cached, store_corrupt_error,
};
use crate::object_persistence::index_to_kio;
use kio_core::cas::{VerifiedCasReadBudget, hash_bytes, is_hash};
use kio_core::dag::{CommitObject, CommitType, TreeObject};
use kio_core::history::{DEFAULT_MAX_HISTORY_TREE_ENTRIES, DEFAULT_MAX_HISTORY_VERIFIED_BYTES};
use kio_core::purge::{EventKind, PurgeState, canonical_final_event, verify_marker_binding};
use kio_core::scope::{DiffChange, DiffEntry, LogReport, Repository};
use kio_core::{KioError, Result};
use kio_index::fts::{
    ExistingSourceIndexOpenMode, FtsSchemaConfig, FtsTokenizer,
    open_existing_source_index_connection,
};
use std::collections::{BTreeMap, BTreeSet};

type Ancestry<'a> = BTreeMap<&'a str, (usize, &'a CommitObject)>;
type Publications = Vec<(String, String, String)>;
type Creations = BTreeMap<(String, String), StoredChunk>;

pub(super) fn diff(repo: &Repository, before: &str, after: &str) -> Result<Vec<DiffEntry>> {
    let mut report = repo.diff(before, after)?;
    VerifiedCasReadBudget::new()
        .run(DEFAULT_MAX_HISTORY_VERIFIED_BYTES, |reads| {
            let repo = repo.with_verified_reads(reads)?;
            // Writers append the ledger before updating SQLite. Capture the
            // projection first, so a concurrent append cannot make it look ahead
            // of the authoritative ledger read below.
            let projected = projected_publications(&repo)?;
            let mut creations = BTreeMap::new();
            let events = visit_chunk_ledger(repo.kio_dir(), true, |mut chunk| {
                chunk.row.text = String::new();
                creations.insert(
                    (
                        chunk.row.chunk_id.clone(),
                        chunk.row.chunking_config_hash.clone(),
                    ),
                    chunk,
                );
                Ok(())
            })?;
            let old_log = repo.log_from(Some(report.before.commit_hash))?;
            let new_log = repo.log_from(Some(report.after.commit_hash))?;
            let old = ancestry(&repo, &old_log)?;
            let new = ancestry(&repo, &new_log)?;
            check_projection(&repo, &projected, &events, [&old, &new])?;
            let mut cache = PublicationAuthenticationCache::default();
            let old_chunks = published_set(
                &repo,
                &report.before.tree,
                &old,
                &events,
                &creations,
                &mut cache,
            )?;
            let new_chunks = published_set(
                &repo,
                &report.after.tree,
                &new,
                &events,
                &creations,
                &mut cache,
            )?;
            if old_chunks != new_chunks {
                report
                    .changes
                    .push(DiffEntry::scope(DiffChange::ChunkSetChanged));
            }
            if resurrection_published(
                &repo,
                [&old, &new],
                [&report.before.tree, &report.after.tree],
            )? {
                report
                    .changes
                    .push(DiffEntry::scope(DiffChange::ResurrectionPublished));
            }
            Ok(report.changes)
        })
        .map(|(changes, _)| changes)
}

fn ancestry<'a>(repo: &Repository, log: &'a LogReport) -> Result<Ancestry<'a>> {
    if log.truncated {
        return Err(store_corrupt_error(
            repo.kio_dir(),
            "commit ancestry for diff is incomplete",
        ));
    }
    Ok(log
        .entries
        .iter()
        .enumerate()
        .map(|(rank, entry)| (entry.commit_hash.as_str(), (rank, &entry.commit)))
        .collect())
}

fn projected_publications(repo: &Repository) -> Result<Publications> {
    let path = sqlite_path(repo.kio_dir());
    if !path
        .try_exists()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Ok(Vec::new());
    }
    let conn = open_existing_source_index_connection(
        &path,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let mut stmt = conn
        .prepare(
            "SELECT chunk_id, chunking_config_hash, introduction_commit FROM chunk_publications",
        )
        .map_err(|error| index_to_kio(error.into()))?;
    let mut rows = stmt.query([]).map_err(|error| index_to_kio(error.into()))?;
    let mut publications = Vec::new();
    while let Some(row) = rows.next().map_err(|error| index_to_kio(error.into()))? {
        if publications.len() as u64 >= DEFAULT_MAX_HISTORY_TREE_ENTRIES {
            return Err(KioError::schema(
                "chunk publication projection read limit exceeded",
            ));
        }
        let hash = |index: usize| {
            row.get_ref(index)
                .ok()
                .and_then(|value| value.as_str().ok())
                .filter(|value| is_hash(value))
                .map(str::to_owned)
                .ok_or_else(|| {
                    store_corrupt_error(&path, "invalid chunk publication projection hash")
                })
        };
        publications.push((hash(0)?, hash(1)?, hash(2)?));
    }
    Ok(publications)
}

fn check_projection(
    repo: &Repository,
    projected: &Publications,
    events: &[ChunkPublicationEvent],
    sides: [&Ancestry<'_>; 2],
) -> Result<()> {
    let durable: BTreeSet<_> = events
        .iter()
        .map(|event| {
            (
                event.chunk_id.as_str(),
                event.chunking_config_hash.as_str(),
                event.introduction_commit.as_str(),
            )
        })
        .collect();
    for (chunk, config, commit) in projected {
        if sides.iter().any(|side| side.contains_key(commit.as_str()))
            && !durable.contains(&(chunk.as_str(), config.as_str(), commit.as_str()))
        {
            return Err(store_corrupt_error(
                &chunks_jsonl_path(repo.kio_dir()),
                "committed chunk publication event is missing",
            ));
        }
    }
    Ok(())
}

fn published_set<'a>(
    repo: &Repository,
    tree: &TreeObject,
    ancestry: &Ancestry<'_>,
    events: &[ChunkPublicationEvent],
    creations: &'a Creations,
    cache: &mut PublicationAuthenticationCache,
) -> Result<BTreeSet<&'a str>> {
    let mut candidates = BTreeMap::<_, Vec<_>>::new();
    for event in events {
        let Some((_, intro)) = ancestry.get(event.introduction_commit.as_str()) else {
            continue;
        };
        let creation = &creations[&(event.chunk_id.clone(), event.chunking_config_hash.clone())];
        match authenticate_publication_event_cached(repo, repo.kio_dir(), event, creation, cache) {
            Ok(false) => {
                return Err(store_corrupt_error(
                    &chunks_jsonl_path(repo.kio_dir()),
                    "committed chunk publication could not be authenticated",
                ));
            }
            // Shallow receipts exclude introductions as in historical search.
            Err(error)
                if error.error_code() == "KIO-E-STORE-NOT-FOUND-001"
                    && matches!(repo.read_tree(&intro.tree), Err(e) if e.error_code() == "KIO-E-STORE-NOT-FOUND-001") =>
            {
                kio_core::gc::GcSweepSession::bind_repository(repo)?
                    .validate_final_shallow_tree(&event.introduction_commit, &intro.tree)?;
                continue;
            }
            Err(error) => return Err(error),
            Ok(true) => {}
        }
        if event.chunking_config_hash == tree.chunking_config_hash {
            let row = &creation.row;
            candidates
                .entry((
                    row.raw_hash.as_str(),
                    row.tool_profile_hash.as_str(),
                    row.r#gen,
                ))
                .or_default()
                .push(row);
        }
    }
    let mut published = BTreeSet::new();
    if candidates.is_empty() {
        return Ok(published);
    }
    let mut checked = BTreeSet::new();
    for entry in &tree.entries {
        let entry = entry?;
        let Some(pin) = entry.normalize else { continue };
        let key = (
            entry.raw_hash.as_str(),
            pin.tool_profile_hash.as_str(),
            pin.r#gen,
        );
        let Some(rows) = candidates.get(&key) else {
            continue;
        };
        if !checked.insert((entry.raw_hash.clone(), pin.clone())) {
            continue;
        }
        let mut units = BTreeSet::new();
        visit_pinned_done_units(repo, &entry.raw_hash, &pin, |unit| {
            units.insert((unit.unit_key, hash_bytes(unit.markdown.as_bytes())));
            Ok(())
        })?;
        published.extend(
            rows.iter()
                .filter(|row| {
                    units.contains(&(row.unit_key.clone(), row.unit_content_hash.clone()))
                })
                .map(|row| row.chunk_id.as_str()),
        );
    }
    Ok(published)
}

fn resurrection_published(
    repo: &Repository,
    sides: [&Ancestry<'_>; 2],
    trees: [&TreeObject; 2],
) -> Result<bool> {
    let path = repo.kio_dir();
    let mut purged = BTreeMap::<_, BTreeSet<_>>::new();
    for (hash, (_, commit)) in sides[0].iter().chain(sides[1]) {
        for raw in &commit.purged_raws {
            purged.entry(raw.as_str()).or_default().insert(*hash);
        }
    }
    if purged.is_empty() {
        return Ok(false);
    }
    if purged.len() as u64 > DEFAULT_MAX_HISTORY_TREE_ENTRIES {
        return Err(KioError::schema("lifecycle diff read limit exceeded"));
    }
    let [old_raws, new_raws] = trees.map(|tree| {
        let mut raws = BTreeSet::new();
        for entry in &tree.entries {
            let entry = entry?;
            if purged.contains_key(entry.raw_hash.as_str()) {
                raws.insert(entry.raw_hash);
            }
        }
        Ok::<_, KioError>(raws)
    });
    let present = [old_raws?, new_raws?];
    let now = repo.control().now_utc_seconds();
    let purge = PurgeState::open_with_control(path, repo.control().clone())?;
    let mut remaining = DEFAULT_MAX_HISTORY_VERIFIED_BYTES;
    let mut published = false;
    for (raw, mut required) in purged {
        let (tombstone, receipt) = purge.read_lifecycle_records(raw, &mut remaining)?;
        let records = [
            tombstone
                .as_ref()
                .map_or(&[][..], |record| record.events.as_slice()),
            receipt
                .as_ref()
                .map_or(&[][..], |record| record.events.as_slice()),
        ];
        for events in records {
            for pair in events.chunks(2) {
                let opening = &pair[0];
                if let Some((_, commit)) = sides
                    .iter()
                    .find_map(|side| side.get(opening.in_commit.as_str()))
                {
                    verify_marker_binding(raw, opening, commit, &now)?;
                    required.remove(opening.in_commit.as_str());
                }
                let Some(retired) = pair.get(1) else { continue };
                for side in sides {
                    let Some((rank, commit)) = side.get(retired.in_commit.as_str()) else {
                        continue;
                    };
                    if side
                        .get(opening.in_commit.as_str())
                        .is_none_or(|(prior, _)| prior <= rank)
                        || commit.commit_type == CommitType::Purged
                    {
                        return Err(store_corrupt_error(
                            path,
                            "invalid resurrection publication ancestry",
                        ));
                    }
                }
                if let Some((_, commit)) = sides[1].get(retired.in_commit.as_str())
                    && !sides[0].contains_key(retired.in_commit.as_str())
                {
                    verify_resurrected_raw(repo, &retired.in_commit, commit, raw)?;
                    published = true;
                }
            }
        }
        if !required.is_empty() {
            return Err(store_corrupt_error(
                path,
                "purge commit is missing its lifecycle event",
            ));
        }
        for (side, raws) in sides.iter().zip(&present) {
            if raws.contains(raw) {
                let [tombstone_tail, receipt_tail] = records.map(|events| {
                    events
                        .iter()
                        .rev()
                        .find(|event| side.contains_key(event.in_commit.as_str()))
                });
                if canonical_final_event(tombstone_tail, receipt_tail)?
                    .is_some_and(|tail| tail.event.kind != EventKind::Retired)
                {
                    return Err(store_corrupt_error(
                        path,
                        "republished raw is missing its retired event",
                    ));
                }
            }
        }
    }
    Ok(published)
}

fn verify_resurrected_raw(
    repo: &Repository,
    hash: &str,
    commit: &CommitObject,
    raw: &str,
) -> Result<()> {
    match repo.read_tree(&commit.tree) {
        Ok(tree) if tree.entries.any(|entry| entry.raw_hash == raw)? => Ok(()),
        Ok(_) => Err(store_corrupt_error(
            repo.kio_dir(),
            "resurrection commit does not contain the republished raw",
        )),
        Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
            kio_core::gc::GcSweepSession::bind_repository(repo)?
                .validate_final_shallow_tree(hash, &commit.tree)
        }
        Err(error) => Err(error),
    }
}
