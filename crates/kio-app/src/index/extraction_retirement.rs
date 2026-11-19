use crate::object_persistence::{bound_extraction_store, normalized_output_ref, pipeline_to_kio};
use kio_core::cas::is_hash;
use kio_core::dag::{NormalizeRef, TreeObject};
use kio_core::scope::Repository;
use kio_core::store_dir::Publication;
use kio_core::{KioError, Result};
use kio_pipeline::task::{EXTRACTION_HOLD_OUTPUT_REF, TaskStatus, TaskStore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

const JOURNAL: &str = "extraction-stages/retirement.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retirement {
    pub(super) raw: String,
    pub(super) path: String,
    pub(super) hold: Option<String>,
    pub(super) normalize: Option<NormalizeRef>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal<R = Vec<Retirement>> {
    head: String,
    parent: Option<String>,
    retire: R,
}

pub(super) fn record(
    repo: &Repository,
    head: &str,
    tree: &TreeObject,
    completed: &[Retirement],
) -> Result<()> {
    let wanted: BTreeSet<_> = completed
        .iter()
        .filter(|item| item.hold.is_some())
        .map(|item| item.path.as_str())
        .collect();
    let mut committed = BTreeMap::new();
    if !wanted.is_empty() {
        for entry in &tree.entries {
            let entry = entry?;
            if wanted.contains(entry.path.as_str()) {
                committed.insert(entry.path, entry.raw_hash);
            }
        }
    }
    let retire = completed
        .iter()
        .filter(|item| item.hold.is_none() || committed.get(&item.path) == Some(&item.raw))
        .collect::<Vec<_>>();
    let directory = bound_extraction_store(repo)?;
    directory.create_directory_all(Path::new("extraction-stages"))?;

    let parent = repo.head_commit_hash()?;
    let journal = Journal {
        head: head.to_owned(),
        parent,
        retire,
    };
    directory.write_atomic_with(Path::new(JOURNAL), Publication::CreateOnly, |file| {
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &journal)?;
        writer.flush()
    })
}

pub(super) fn has_journal(repo: &Repository) -> Result<bool> {
    let directory = bound_extraction_store(repo)?;
    let journal = Path::new(JOURNAL);
    let parent = journal.parent().expect("retirement journal directory");
    if !directory.contains_entry(parent)? {
        return Ok(false);
    }
    let stages = kio_core::store_dir::StoreDirectory::from_retained_with_control(
        directory.open_directory(parent)?,
        directory.path().join(parent),
        directory.control().clone(),
    )?;
    stages.contains_entry(Path::new(
        journal.file_name().expect("retirement journal leaf"),
    ))
}

pub(super) fn recover(repo: &Repository) -> Result<bool> {
    recover_with(repo, |_| Ok(()))
}

pub(super) fn recover_for_index(repo: &Repository) -> Result<bool> {
    recover_with(repo, |journal| {
        let (_, before) = super::command::dispositions_at_head(repo, journal.parent.as_deref())?;
        let (_, pending) = super::command::dispositions_at_head(repo, Some(&journal.head))?;
        if before != pending {
            // The pending publication owns an unsupported-input change. It must
            // be resolved before a new index can record additional observations.
            let _lock = repo.lock_store_for_publication()?;
        }
        Ok(())
    })
}

fn recover_with(
    repo: &Repository,
    before_history_recovery: impl FnOnce(&Journal) -> Result<()>,
) -> Result<bool> {
    let directory = bound_extraction_store(repo)?;
    let path = Path::new(JOURNAL);
    let parsed = directory.read_optional_with(path, u64::MAX, |reader| {
        serde_json::from_reader(BufReader::new(reader))
            .map_err(|error| KioError::schema(error.to_string()))
    })?;
    let Some(journal): Option<Journal> = parsed else {
        return Ok(false);
    };
    if !is_hash(&journal.head)
        || journal
            .parent
            .as_deref()
            .is_some_and(|parent| !is_hash(parent))
        || journal.retire.iter().any(|item| !is_hash(&item.raw))
    {
        return Err(KioError::schema("invalid extraction retirement identity"));
    }
    repo.recover_extraction_publication(&journal.head, || before_history_recovery(&journal))?;
    let mut current = repo.head_commit_hash()?;
    while let Some(head) = current {
        if head == journal.head {
            retire(repo, &directory, &journal.retire)?;
            directory.remove_file(path)?;
            return Ok(true);
        }
        if Some(&head) == journal.parent.as_ref() {
            break;
        }
        current = repo.read_commit(&head)?.parent;
    }
    directory.remove_file(path)?;
    Ok(false)
}

fn retire(
    repo: &Repository,
    directory: &kio_core::store_dir::StoreDirectory,
    completed: &[Retirement],
) -> Result<()> {
    if completed.is_empty() {
        return Ok(());
    }
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let holds: BTreeMap<_, _> = completed
        .iter()
        .filter_map(|item| item.hold.as_deref().map(|id| (id, item)))
        .collect();
    if !holds.is_empty() {
        store
            .update_matching(|task| {
                let Some(item) = holds.get(task.task_id.as_str()) else {
                    return false;
                };
                if task.input_hash != item.raw
                    || task.input_path != item.path
                    || task.output_ref != EXTRACTION_HOLD_OUTPUT_REF
                    || task.resource_limit.is_none()
                {
                    return false;
                }
                if let Some(normalize) = &item.normalize {
                    task.output_ref = normalized_output_ref(
                        repo,
                        &item.raw,
                        &normalize.tool_profile_hash,
                        normalize.r#gen,
                    );
                    task.status = TaskStatus::Done;
                } else {
                    task.status = TaskStatus::Skipped;
                }
                task.resource_limit = None;
                task.hold_reason = None;
                task.fallback_reason = Some("extraction_completed".to_owned());
                true
            })
            .map_err(pipeline_to_kio)?;
    }
    repo.control()
        .checkpoint(kio_core::durability::DurabilityPoint::ExtractionBeforeRetirementCleanup)?;
    let held_raws: BTreeSet<_> = store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.output_ref == EXTRACTION_HOLD_OUTPUT_REF && task.resource_limit.is_some()
        })
        .map(|task| task.input_hash)
        .collect();
    let raws: BTreeSet<_> = completed.iter().map(|item| item.raw.as_str()).collect();
    for raw in raws {
        if !held_raws.contains(raw) {
            kio_pipeline::prepare::discard_prepared_stages(directory, raw)
                .map_err(pipeline_to_kio)?;
            kio_pipeline::markdownize::discard_normalized_stages(directory, raw)
                .map_err(pipeline_to_kio)?;
        }
    }
    Ok(())
}
