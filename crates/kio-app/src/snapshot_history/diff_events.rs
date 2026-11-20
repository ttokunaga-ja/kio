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
use kio_core::purge::{EventKind, PurgeState, verify_marker_binding};
use kio_core::scope::{DiffChange, DiffEntry, LogReport, Repository};
use kio_core::{ExitCode, KioError, Result};
use kio_index::fts::{
    ExistingSourceIndexOpenMode, FtsSchemaConfig, FtsTokenizer,
    open_existing_source_index_connection,
};
use kio_pipeline::PipelineError;
use std::collections::{BTreeMap, BTreeSet};

type Ancestry<'a> = BTreeMap<&'a str, (usize, &'a CommitObject)>;
type Publications = Vec<(String, String, String)>;
type Creations = BTreeMap<(String, String), StoredChunk>;
type Associations<'a> = BTreeMap<(&'a str, &'a str), BTreeSet<&'a str>>;
type UnitGroup<'a> = (&'a str, &'a str, u64, &'a str, &'a str);
type LifecycleUpdates<'a> = BTreeMap<String, Vec<(&'a str, (u64, bool), EventKind)>>;

pub(super) fn diff(repo: &Repository, before: &str, after: &str) -> Result<Vec<DiffEntry>> {
    diff_with_limit(repo, before, after, DEFAULT_MAX_HISTORY_VERIFIED_BYTES)
}

fn diff_with_limit(
    repo: &Repository,
    before: &str,
    after: &str,
    max_bytes: u64,
) -> Result<Vec<DiffEntry>> {
    VerifiedCasReadBudget::new()
        .run(max_bytes, |reads| {
            let repo = repo.with_verified_reads(reads)?;
            let mut report = repo.diff(before, after)?;
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
            let trees = [&report.before.tree, &report.after.tree];
            let associations =
                verified_associations(&repo, trees, [&old, &new], &events, &creations)?;
            let old_chunks = published_set(&repo, trees[0], &old, &creations, &associations)?;
            let new_chunks = published_set(&repo, trees[1], &new, &creations, &associations)?;
            if old_chunks != new_chunks {
                report
                    .changes
                    .push(DiffEntry::scope(DiffChange::ChunkSetChanged));
            }
            if resurrection_published(&repo, [&old, &new], [&old_log, &new_log])? {
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

// A selected pin can be covered by an introduction outside its ancestry.
// Authenticate it for completeness without adding it to that snapshot.
fn verified_associations<'a>(
    repo: &Repository,
    trees: [&TreeObject; 2],
    sides: [&Ancestry<'_>; 2],
    events: &'a [ChunkPublicationEvent],
    creations: &Creations,
) -> Result<Associations<'a>> {
    let mut selected = BTreeSet::new();
    for tree in trees {
        for entry in &tree.entries {
            let entry = entry?;
            if let Some(pin) = entry.normalize {
                selected.insert((
                    tree.chunking_config_hash.as_str(),
                    entry.raw_hash,
                    pin.tool_profile_hash,
                    pin.r#gen,
                ));
            }
        }
    }
    let mut cache = PublicationAuthenticationCache::default();
    let mut associations = Associations::new();
    for event in events {
        let creation = &creations[&(event.chunk_id.clone(), event.chunking_config_hash.clone())];
        let row = &creation.row;
        let selected_commit = sides
            .iter()
            .find_map(|side| side.get(event.introduction_commit.as_str()));
        if !selected.contains(&(
            event.chunking_config_hash.as_str(),
            row.raw_hash.clone(),
            row.tool_profile_hash.clone(),
            row.r#gen,
        )) {
            continue;
        }
        match authenticate_publication_event_cached(
            repo,
            repo.kio_dir(),
            event,
            creation,
            &mut cache,
        ) {
            Ok(true) => {
                associations
                    .entry((&event.chunk_id, &event.chunking_config_hash))
                    .or_default()
                    .insert(&event.introduction_commit);
            }
            Ok(false) if selected_commit.is_none() => {} // Uncommitted append residue.
            Ok(false) => {
                return Err(store_corrupt_error(
                    &chunks_jsonl_path(repo.kio_dir()),
                    "committed chunk publication could not be authenticated",
                ));
            }
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                let intro = repo.read_commit(&event.introduction_commit)?;
                read_history_tree(repo, &event.introduction_commit, &intro.tree)?;
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(associations)
}

fn published_set<'a>(
    repo: &Repository,
    tree: &TreeObject,
    ancestry: &Ancestry<'_>,
    creations: &'a Creations,
    associations: &Associations<'_>,
) -> Result<BTreeSet<&'a str>> {
    let mut rows_by_unit = BTreeMap::<UnitGroup<'_>, Vec<_>>::new();
    for creation in creations.values() {
        let row = &creation.row;
        if row.chunking_config_hash == tree.chunking_config_hash {
            rows_by_unit
                .entry((
                    &row.raw_hash,
                    &row.tool_profile_hash,
                    row.r#gen,
                    &row.unit_key,
                    &row.unit_content_hash,
                ))
                .or_default()
                .push(row);
        }
    }
    for rows in rows_by_unit.values_mut() {
        rows.sort_unstable_by_key(|row| (row.byte_start, row.byte_end));
    }
    let path = chunks_jsonl_path(repo.kio_dir());
    let mut published = BTreeSet::new();
    let mut checked = BTreeSet::new();
    for entry in &tree.entries {
        let entry = entry?;
        let Some(pin) = entry.normalize else { continue };
        if !checked.insert((entry.raw_hash.clone(), pin.clone())) {
            continue;
        }
        visit_pinned_done_units(repo, &entry.raw_hash, &pin, |unit| {
            let hash = hash_bytes(unit.markdown.as_bytes());
            let key = (
                entry.raw_hash.as_str(),
                pin.tool_profile_hash.as_str(),
                pin.r#gen,
                unit.unit_key.as_str(),
                hash.as_str(),
            );
            let mut covered = 0;
            let incomplete = || {
                PipelineError::corrupt(
                    path.display().to_string(),
                    "cannot establish complete publications for a pinned normalized unit",
                )
            };
            // Heading chunking covers the whole unit except whitespace-only spans.
            // Check completeness independently of SQLite, then apply ancestry to
            // the verified publications. A later introduction proves the chunk
            // exists without publishing it retroactively.
            for row in rows_by_unit.get(&key).into_iter().flatten() {
                let introductions = associations
                    .get(&(row.chunk_id.as_str(), row.chunking_config_hash.as_str()))
                    .ok_or_else(incomplete)?;
                let start = usize::try_from(row.byte_start).map_err(|_| incomplete())?;
                let end = usize::try_from(row.byte_end).map_err(|_| incomplete())?;
                if !unit
                    .markdown
                    .get(covered..start)
                    .is_some_and(|gap| gap.trim().is_empty())
                    || !unit.markdown.get(start..end).is_some_and(|span| {
                        !span.trim().is_empty() && hash_bytes(span.as_bytes()) == row.text_hash
                    })
                {
                    return Err(incomplete());
                }
                covered = end;
                if introductions.iter().any(|hash| ancestry.contains_key(hash)) {
                    published.insert(row.chunk_id.as_str());
                }
            }
            if !unit.markdown[covered..].trim().is_empty() {
                return Err(incomplete());
            }
            Ok(())
        })?;
    }
    Ok(published)
}

fn resurrection_published(
    repo: &Repository,
    sides: [&Ancestry<'_>; 2],
    logs: [&LogReport; 2],
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
    let now = repo.control().now_utc_seconds();
    let purge = PurgeState::open_with_control(path, repo.control().clone())?;
    let mut remaining = DEFAULT_MAX_HISTORY_VERIFIED_BYTES;
    let mut updates = LifecycleUpdates::new();
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
        for (source, events) in records.into_iter().enumerate() {
            for event in events {
                if sides
                    .iter()
                    .any(|side| side.contains_key(event.in_commit.as_str()))
                {
                    let order = (
                        event.lifecycle_epoch.expect("validated lifecycle event"),
                        source == 0,
                    );
                    updates
                        .entry(event.in_commit.clone())
                        .or_default()
                        .push((raw, order, event.kind));
                }
            }
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
    }
    for (log, other) in logs.into_iter().zip([sides[1], sides[0]]) {
        verify_retirement_coverage(repo, log, other, &updates)?;
    }
    Ok(published)
}

fn verify_retirement_coverage(
    repo: &Repository,
    history: &LogReport,
    other: &Ancestry<'_>,
    updates: &LifecycleUpdates<'_>,
) -> Result<()> {
    let mut latest = BTreeMap::new();
    let mut active = BTreeSet::new();
    for (rank, entry) in history.entries.iter().enumerate().rev() {
        for &(raw, order, kind) in updates.get(&entry.commit_hash).into_iter().flatten() {
            let prior = latest.entry(raw).or_default();
            if order > *prior {
                *prior = order;
                if kind == EventKind::Retired {
                    active.remove(raw);
                } else {
                    active.insert(raw);
                }
            }
        }
        // Establish the lifecycle state from the shared history, then check
        // raw presence at the endpoints and within the comparison interval.
        if (rank == 0 || !other.contains_key(entry.commit_hash.as_str()))
            && !active.is_empty()
            && read_history_tree(repo, &entry.commit_hash, &entry.commit.tree)?
                .entries
                .any(|item| active.contains(item.raw_hash.as_str()))?
        {
            return Err(store_corrupt_error(
                repo.kio_dir(),
                "republished raw is missing its retired event",
            ));
        }
    }
    Ok(())
}

fn verify_resurrected_raw(
    repo: &Repository,
    hash: &str,
    commit: &CommitObject,
    raw: &str,
) -> Result<()> {
    if !read_history_tree(repo, hash, &commit.tree)?
        .entries
        .any(|entry| entry.raw_hash == raw)?
    {
        return Err(store_corrupt_error(
            repo.kio_dir(),
            "resurrection commit does not contain the republished raw",
        ));
    }
    Ok(())
}

fn read_history_tree(repo: &Repository, commit: &str, tree: &str) -> Result<TreeObject> {
    match repo.read_tree(tree) {
        Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
            kio_core::gc::GcSweepSession::bind_repository(repo)?
                .validate_final_shallow_tree(commit, tree)?;
            Err(KioError::new(
                "KIO-E-COMMIT-SHALLOW-001",
                "cannot verify diff events through a shallow commit",
                serde_json::json!({"commit_hash": commit, "tree_hash": tree}),
                ExitCode::Failure,
            ))
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kio_core::cas::ObjectKind;
    use serde_json::json;

    #[test]
    fn diff_read_limit_includes_both_endpoint_trees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        std::fs::write(dir.path().join("file.txt"), b"original").unwrap();
        let base = repo.snapshot(None, None).unwrap().commit_hash.unwrap();
        let mut commit = repo.read_commit(&base).unwrap();
        let mut tree = serde_json::to_value(repo.read_tree(&commit.tree).unwrap()).unwrap();
        let raw = tree["entries"][0]["raw_hash"].clone();
        let entries = tree["entries"].as_array_mut().unwrap();
        for index in 0..100 {
            entries.push(
                json!({"path": format!("name{index:03}.txt"), "type": "file", "raw_hash": raw}),
            );
        }
        let store = repo.object_store();
        (commit.tree, _) = store.write_json(ObjectKind::Tree, &tree).unwrap();
        commit.parent = Some(base.clone());
        let (large, _) = store
            .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
            .unwrap();
        for (before, after) in [(&base, &large), (&large, &base)] {
            let error = diff_with_limit(&repo, before, after, 8192).unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
        }
    }
}
