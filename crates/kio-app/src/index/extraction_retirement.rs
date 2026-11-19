use crate::object_persistence::{bound_extraction_store, normalized_output_ref, pipeline_to_kio};
use kio_core::cas::is_hash;
use kio_core::dag::{NormalizeRef, TreeObject};
use kio_core::scope::Repository;
use kio_core::store_dir::Publication;
use kio_core::{KioError, Result};
use kio_pipeline::task::{EXTRACTION_HOLD_OUTPUT_REF, TaskStatus, TaskStore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const JOURNAL: &str = "extraction-stages/retirement.json";
// A snapshot has at most MAX_TREE_ENTRIES direct files.
const JOURNAL_BYTES: u64 = 32 * 1024 * 1024;

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
struct Journal {
    head: String,
    parent: Option<String>,
    retire: Vec<Retirement>,
}

pub(super) fn record(
    repo: &Repository,
    head: &str,
    tree: &TreeObject,
    completed: &[Retirement],
) -> Result<()> {
    let committed: BTreeMap<_, _> = tree
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.raw_hash.as_str()))
        .collect();
    let retire = completed
        .iter()
        .filter(|item| {
            item.hold.is_none() || committed.get(item.path.as_str()) == Some(&item.raw.as_str())
        })
        .map(|item| Retirement {
            raw: item.raw.clone(),
            path: item.path.clone(),
            hold: item.hold.clone(),
            normalize: item.normalize.clone(),
        })
        .collect::<Vec<_>>();
    let directory = bound_extraction_store(repo)?;
    directory.create_directory_all(Path::new("extraction-stages"))?;

    let parent = repo.head_commit_hash()?;
    let bytes = serde_json::to_vec(&Journal {
        head: head.to_owned(),
        parent,
        retire,
    })
    .map_err(|error| KioError::schema(error.to_string()))?;
    directory.write_atomic(Path::new(JOURNAL), &bytes, Publication::CreateOnly)
}

pub(super) fn has_journal(repo: &Repository) -> Result<bool> {
    bound_extraction_store(repo)?
        .read_optional(Path::new(JOURNAL), JOURNAL_BYTES)
        .map(|bytes| bytes.is_some())
}

pub(super) fn recover(repo: &Repository) -> Result<bool> {
    let directory = bound_extraction_store(repo)?;
    let path = Path::new(JOURNAL);
    let Some(bytes) = directory.read_optional(path, JOURNAL_BYTES)? else {
        return Ok(false);
    };
    let journal: Journal =
        serde_json::from_slice(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
    if !is_hash(&journal.head)
        || journal
            .parent
            .as_deref()
            .is_some_and(|parent| !is_hash(parent))
        || journal.retire.iter().any(|item| !is_hash(&item.raw))
    {
        return Err(KioError::schema("invalid extraction retirement identity"));
    }
    repo.recover_extraction_publication(&journal.head)?;
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
