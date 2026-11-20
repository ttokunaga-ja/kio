use crate::object_persistence::{bound_extraction_store, normalized_output_ref, pipeline_to_kio};
use kio_core::cas::is_hash;
use kio_core::dag::{NormalizeRef, TreeObject};
use kio_core::disk_records::{
    DiskRecords, RecordSorter, RecordWriter, SortedRecordCursor, decode_bounded_json,
};
use kio_core::scope::{DeferredInput, Repository};
use kio_core::store_dir::Publication;
use kio_core::{KioError, Result};
use kio_pipeline::task::{EXTRACTION_HOLD_OUTPUT_REF, TaskStatus, TaskStore};
use serde::{Deserialize, Serialize};
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
struct Journal {
    head: String,
    parent: Option<String>,
    retire: DiskRecords<Retirement>,
}

/// Record only adopted work for a scan. When deferrals are present, both streams
/// are ordered by path. A prior HEAD entry can have the same raw hash as a
/// deferred extraction without adopting its newly finished normalization.
pub(super) fn record(
    repo: &Repository,
    head: &str,
    tree: &TreeObject,
    completed: &DiskRecords<Retirement>,
    deferred: &DiskRecords<DeferredInput>,
) -> Result<()> {
    let stages = repo.object_store().record_stages()?;
    let mut by_path = RecordSorter::new(&stages, |a: &Retirement, b| a.path.cmp(&b.path));
    let mut retire = RecordWriter::new(&stages)?;
    let mut deferred = SortedRecordCursor::new(deferred.iter(), |row| &row.path);
    for item in completed {
        let item = item?;
        if deferred.at(&item.path)?.is_some() {
            continue;
        }
        if item.hold.is_none() {
            retire.push(&item)?;
        } else {
            by_path.push(item)?;
        }
    }
    let paths = by_path.finish()?;
    if !paths.is_empty() {
        let mut tree = tree.entries.iter();
        let mut entry = tree.next().transpose()?;
        for item in &paths {
            let item = item?;
            while entry.as_ref().is_some_and(|entry| entry.path < item.path) {
                entry = tree.next().transpose()?;
            }
            if entry
                .as_ref()
                .is_some_and(|entry| entry.path == item.path && entry.raw_hash == item.raw)
            {
                retire.push(&item)?;
            }
        }
    }
    let retire = retire.finish()?;
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
    let stages = repo.object_store().record_stages()?;
    let parsed = directory.read_optional_with(path, u64::MAX, |reader| {
        decode_bounded_json(&stages, BufReader::new(reader))
    })?;
    let Some(journal): Option<Journal> = parsed else {
        return Ok(false);
    };
    if !is_hash(&journal.head)
        || journal
            .parent
            .as_deref()
            .is_some_and(|parent| !is_hash(parent))
    {
        return Err(KioError::schema("invalid extraction retirement identity"));
    }
    for item in &journal.retire {
        if !is_hash(&item?.raw) {
            return Err(KioError::schema(
                "invalid extraction retirement raw identity",
            ));
        }
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
    completed: &DiskRecords<Retirement>,
) -> Result<()> {
    if completed.is_empty() {
        return Ok(());
    }
    let stages = repo.object_store().record_stages()?;
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let mut holds = RecordSorter::new(&stages, |a: &Retirement, b| a.hold.cmp(&b.hold));
    let mut raws = RecordSorter::new(&stages, String::cmp);
    for item in completed {
        let item = item?;
        raws.push(item.raw.clone())?;
        if item.hold.is_some() {
            holds.push(item)?;
        }
    }
    let holds = holds.finish()?;
    let mut hold_iter = holds.iter();
    let mut hold = hold_iter.next().transpose()?;
    let mut held_raws = RecordSorter::new(&stages, String::cmp);
    let tasks = store
        .sorted_bound(directory, &stages)
        .map_err(pipeline_to_kio)?;
    let updated = tasks.iter().map(|task| -> Result<_> {
        let mut task = task?;
        while hold
            .as_ref()
            .and_then(|item| item.hold.as_ref())
            .is_some_and(|id| id < &task.task_id)
        {
            hold = hold_iter.next().transpose()?;
        }
        let is_held =
            task.output_ref == EXTRACTION_HOLD_OUTPUT_REF && task.resource_limit.is_some();
        if is_held {
            if let Some(item) = hold.as_ref().filter(|item| {
                item.hold.as_ref() == Some(&task.task_id)
                    && item.raw == task.input_hash
                    && item.path == task.input_path
            }) {
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
            } else {
                held_raws.push(task.input_hash.clone())?;
            }
        }
        Ok(task)
    });
    if holds.is_empty() {
        for task in updated {
            task?;
        }
    } else {
        store
            .replace_sorted_bound(directory, updated)
            .map_err(pipeline_to_kio)?;
    }
    repo.control()
        .checkpoint(kio_core::durability::DurabilityPoint::ExtractionBeforeRetirementCleanup)?;
    let held = held_raws.finish()?;
    let mut held_iter = held.iter();
    let mut held_raw = held_iter.next().transpose()?;
    let mut previous = None;
    for raw in &raws.finish()? {
        let raw = raw?;
        if previous.as_ref() == Some(&raw) {
            continue;
        }
        while held_raw.as_ref().is_some_and(|held| held < &raw) {
            held_raw = held_iter.next().transpose()?;
        }
        if held_raw.as_ref() != Some(&raw) {
            kio_pipeline::prepare::discard_prepared_stages(directory, &raw)
                .map_err(pipeline_to_kio)?;
            kio_pipeline::markdownize::discard_normalized_stages(directory, &raw)
                .map_err(pipeline_to_kio)?;
        }
        previous = Some(raw);
    }
    Ok(())
}
