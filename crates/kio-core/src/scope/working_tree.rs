use super::*;
use crate::dag::TreeBuilder;
use crate::disk_records::{DiskRecords, RecordSorter, RecordWriter, SortedRecordCursor};

pub(super) struct WorkingCandidates {
    names: DiskRecords<String>,
    root: PathBuf,
    bound_root: Option<Arc<File>>,
}
impl WorkingCandidates {
    pub(super) fn iter(&self) -> impl Iterator<Item = Result<WorkingFileCandidate>> + '_ {
        self.names.iter().map(|name| {
            let file_name = name?;
            Ok(WorkingFileCandidate {
                path: self.root.join(&file_name),
                file_name,
                bound_root: self.bound_root.clone(),
            })
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ArchivedFile {
    name: String,
    size: u64,
    source: crate::platform::ScheduledSourceIdentity,
    stage: crate::cas::SavedRawStage,
}
type StagedInput = (
    crate::cas::BoundRawStage,
    crate::platform::ScheduledSourceIdentity,
);

struct ArchiveBatch<'a> {
    records: RecordWriter<ArchivedFile>,
    store: &'a ObjectStore,
}
impl Drop for ArchiveBatch<'_> {
    fn drop(&mut self) {
        for record in self.records.iter() {
            let Ok(record) = record else {
                break;
            };
            if let Ok(stage) = self.store.reopen_saved_stage(&record.stage) {
                drop(stage);
            }
        }
    }
}

impl Repository {
    pub fn build_working_tree(&self, store_raw: bool) -> Result<WorkingTree> {
        self.build_working_tree_filtered(store_raw, &BTreeSet::new())
    }

    pub fn build_working_tree_filtered(
        &self,
        store_raw: bool,
        excluded_paths: &BTreeSet<String>,
    ) -> Result<WorkingTree> {
        self.build_working_tree_with_normalize(store_raw, excluded_paths, &BTreeMap::new())
    }

    pub fn build_working_tree_with_normalize(
        &self,
        store_raw: bool,
        excluded_paths: &BTreeSet<String>,
        normalize_by_path: &BTreeMap<String, NormalizeRef>,
    ) -> Result<WorkingTree> {
        if !normalize_by_path.is_empty() {
            return Err(KioError::invalid_usage(
                "normalization metadata must include its expected raw hash",
            ));
        }
        self.build_working_tree_with_limits(
            store_raw,
            excluded_paths,
            normalize_by_path,
            ArchiveLimits::default(),
        )
    }

    pub fn build_working_tree_with_limits(
        &self,
        store_raw: bool,
        excluded_paths: &BTreeSet<String>,
        normalize_by_path: &BTreeMap<String, NormalizeRef>,
        limits: ArchiveLimits,
    ) -> Result<WorkingTree> {
        if !normalize_by_path.is_empty() {
            return Err(KioError::invalid_usage(
                "normalization metadata must include its expected raw hash",
            ));
        }
        self.build_working_tree_with_bound_normalize_and_limits(
            store_raw,
            excluded_paths,
            &BTreeMap::new(),
            &BTreeSet::new(),
            limits,
        )
    }

    pub fn build_working_tree_with_bound_normalize_and_limits(
        &self,
        store_raw: bool,
        excluded_paths: &BTreeSet<String>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
        limits: ArchiveLimits,
    ) -> Result<WorkingTree> {
        let limits = if store_raw {
            limits
        } else {
            ArchiveLimits::default()
        };
        let chunking_config_hash = self.effective_chunking_config_hash()?;

        let candidates = self.working_file_candidates(
            &|path| !excluded_paths.contains(path),
            explicitly_allowed_tier_a_paths,
            store_raw,
            limits,
        )?;
        if store_raw {
            // Raw publication and erase-receipt retirement are one store-locked
            // operation even for direct callers of this public builder. Snapshot
            // callers already hold this reentrant lock.
            let _lock = self.lock_store_in_session(&self.lock_session)?;
            // Every caller of this public builder except `snapshot_with_type`'s
            // purge path is a non-purge write (`purge_self_targets` empty —
            // see `archive_staged_working_tree`'s doc comment): the barrier
            // applies unconditionally here, as it always has.
            return self.archive_staged_working_tree(
                candidates,
                &mut |path| Ok(normalize_by_path.get(path).cloned()),
                limits,
                &BTreeSet::new(),
                None,
                None,
                None,
                &chunking_config_hash,
            );
        }
        self.read_working_tree(
            candidates,
            normalize_by_path,
            &DiskRecords::empty(),
            chunking_config_hash,
            limits,
        )
    }

    fn read_working_tree(
        &self,
        candidates: WorkingCandidates,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        deferred: &DiskRecords<DeferredInput>,
        chunking_config_hash: String,
        limits: ArchiveLimits,
    ) -> Result<WorkingTree> {
        let mut deferred = SortedRecordCursor::new(deferred.iter(), |row| &row.path);
        let mut entries = TreeBuilder::new(&self.store.record_stages()?, chunking_config_hash);
        let mut consumed_scope_bytes = 0_u64;
        let mut count = 0;
        for candidate in candidates.iter() {
            let candidate = candidate?;
            if deferred.at(&candidate.file_name)?.is_some() {
                continue;
            }
            if count == TREE_ENTRY_WARNING_THRESHOLD {
                warn_large_working_tree();
            }
            count += 1;
            let mut file = open_working_file_candidate(&candidate, self.control())?;
            let metadata = file.metadata().kio_io(&candidate.path)?;
            let remaining = limits
                .max_scope_bytes
                .checked_sub(consumed_scope_bytes)
                .ok_or_else(|| scope_total_oversized(&candidate.file_name, limits, u64::MAX))?;
            let allowed = limits.max_file_bytes.min(remaining);
            if metadata.len() > allowed {
                return Err(scope_input_oversized(
                    &candidate.file_name,
                    limits,
                    metadata.len(),
                ));
            }
            let (raw_hash, consumed) = hash_scope_file(
                &mut file,
                &candidate.path,
                &candidate.file_name,
                allowed,
                limits,
            )?;
            if file.metadata().kio_io(&candidate.path)?.len() != consumed {
                return Err(scope_file_changed(&candidate.file_name));
            }
            consumed_scope_bytes = consumed_scope_bytes
                .checked_add(consumed)
                .ok_or_else(|| scope_total_oversized(&candidate.file_name, limits, u64::MAX))?;
            let mut tree_entry = TreeEntry::raw_file(candidate.file_name.clone(), raw_hash)?;
            if let Some(pending) = normalize_by_path.get(&candidate.file_name) {
                if pending.expected_raw_hash == tree_entry.raw_hash {
                    tree_entry.normalize = Some(pending.normalize.clone());
                } else {
                    eprintln!(
                        "warning: normalization metadata not attached because {} changed after normalization",
                        candidate.file_name
                    );
                }
            }
            tree_entry.validate()?;
            entries.push(tree_entry)?;
        }
        Ok(WorkingTree {
            tree: entries.finish()?,
        })
    }

    pub fn validate_scanned_archive_budget(
        &self,
        inputs: &DiskRecords<ScannedInput>,
    ) -> Result<()> {
        let limits = self.archive_limits;
        let mut consumed = 0u64;
        for input in inputs {
            let input = input?;
            let name = &input.path;
            if input.raw_hash.is_none() {
                continue;
            }
            if input.size_bytes > limits.max_file_bytes {
                return Err(scope_input_oversized(name, limits, input.size_bytes));
            }
            consumed = consumed
                .checked_add(input.size_bytes)
                .ok_or_else(|| scope_total_oversized(name, limits, u64::MAX))?;
            if consumed > limits.max_scope_bytes {
                return Err(scope_total_oversized(name, limits, consumed));
            }
        }
        Ok(())
    }

    pub(super) fn archive_scanned_working_tree(
        &self,
        inputs: &DiskRecords<ScannedInput>,
        deferred: &mut DiskRecords<DeferredInput>,
        prior_tree: Option<&TreeObject>,
        normalizes: &DiskRecords<(String, PendingNormalizeRef)>,
        chunking_config_hash: &str,
    ) -> Result<WorkingTree> {
        let root = self.bound_root.as_deref().ok_or_else(|| {
            KioError::invalid_usage("scanned archive requires a retained scope capability")
        })?;
        self.validate_scanned_archive_budget(inputs)?;
        if inputs.len() > TREE_ENTRY_WARNING_THRESHOLD {
            warn_large_working_tree();
        }
        self.store.cleanup_bound_raw_stages()?;
        let record_stages = self.store.record_stages()?;
        let mut batch = ArchiveBatch {
            records: RecordWriter::new(&record_stages)?,
            store: &self.store,
        };
        let mut new_deferrals = RecordWriter::new(&record_stages)?;
        let mut prior_deferrals = SortedRecordCursor::new(deferred.iter(), |row| &row.path);
        for input in inputs {
            let input = input?;
            let name = input.path;
            if prior_deferrals.at(&name)?.is_some() {
                continue;
            }
            let staged = match input.raw_hash.as_deref() {
                Some(raw_hash) => {
                    self.stage_scanned_input(root, &name, raw_hash, input.size_bytes)?
                }
                None => Err(DeferredInputReason::UnstableDuringScan),
            };
            match staged {
                Ok((mut stage, source)) => {
                    batch.records.push(&ArchivedFile {
                        name,
                        size: stage.size_bytes(),
                        source,
                        stage: stage.saved(),
                    })?;
                    stage.detach();
                }
                Err(reason) => new_deferrals.push(&DeferredInput { path: name, reason })?,
            }
        }
        let new_deferrals = new_deferrals.finish()?;
        if deferred.is_empty() {
            *deferred = new_deferrals;
        } else if !new_deferrals.is_empty() {
            let mut combined = RecordWriter::new(&record_stages)?;
            for row in crate::disk_records::merge_sorted_records(
                deferred.iter(),
                new_deferrals.iter(),
                |a, b| a.path.cmp(&b.path),
            ) {
                combined.push(&row?)?;
            }
            *deferred = combined.finish()?;
        }

        let purge = self.purge_state()?;
        let mut entries = TreeBuilder::new(&record_stages, chunking_config_hash.to_owned());
        if !deferred.is_empty()
            && let Some(prior) = prior_tree
        {
            let mut deferred_paths = SortedRecordCursor::new(deferred.iter(), |row| &row.path);
            for entry in &prior.entries {
                let entry = entry?;
                if deferred_paths.at(&entry.path)?.is_some() {
                    ensure_raw_publication_allowed(&purge, &entry.raw_hash)?;
                    entries.push(entry)?;
                }
            }
        }
        let mut normalizes = SortedRecordCursor::new(normalizes.iter(), |row| &row.0);
        self.publish_archive_batch(
            &batch,
            &mut entries,
            &mut |path| Ok(normalizes.at(path)?.map(|(_, pending)| pending.clone())),
            &BTreeSet::new(),
            Some(&purge),
        )?;
        Ok(WorkingTree {
            tree: entries.finish()?,
        })
    }

    fn stage_scanned_input(
        &self,
        root: &File,
        name: &str,
        expected_hash: &str,
        expected_size: u64,
    ) -> Result<std::result::Result<StagedInput, DeferredInputReason>> {
        use DeferredInputReason::ChangedAfterScan;

        let path = self.canonical_root.join(name);
        let file = open_bound_scope_file_nofollow(root, name, &path, self.control());
        let mut file = match file {
            Ok(file) => file,
            Err(error) => return Ok(Err(scanned_open_failure(root, name, error)?)),
        };
        let before = file.metadata().kio_io(&path)?;
        if before.len() != expected_size {
            return Ok(Err(ChangedAfterScan));
        }
        let source = scheduled_source_identity(&file)?;
        // Read at most one byte beyond the scan's size, even for a growing log.
        let limit = expected_size.saturating_add(1);
        let stage = self
            .store
            .stage_raw_from_reader(&mut (&mut file).take(limit), limit)
            .map_err(|error| {
                error
                    .with_context_field("path", json!(name))
                    .with_context_field("saved", json!(false))
            })?;
        if stage.size_bytes() != expected_size || stage.raw_hash() != expected_hash {
            return Ok(Err(ChangedAfterScan));
        }
        let current = match open_bound_scope_file_nofollow(root, name, &path, self.control()) {
            Ok(file) => file,
            Err(error) => return Ok(Err(scanned_open_failure(root, name, error)?)),
        };
        let after = current.metadata().kio_io(&path)?;
        if after.len() != expected_size
            || after.modified().kio_io(&path)? != before.modified().kio_io(&path)?
            || scheduled_source_identity(&current)? != source
        {
            return Ok(Err(ChangedAfterScan));
        }
        Ok(Ok((stage, source)))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn archive_staged_working_tree(
        &self,
        candidates: WorkingCandidates,
        normalize_for_path: &mut dyn FnMut(&str) -> Result<Option<PendingNormalizeRef>>,
        limits: ArchiveLimits,
        purge_self_targets: &BTreeSet<String>,
        expected_raw_by_path: Option<&BTreeMap<String, String>>,
        expected_direct_entries: Option<&BTreeSet<String>>,
        expected_snapshot_policy: Option<&SnapshotAutoBinding>,
        chunking_config_hash: &str,
    ) -> Result<WorkingTree> {
        let scheduled_bound = expected_raw_by_path.is_some() && self.bound_kio.is_some();
        self.store.cleanup_bound_raw_stages()?;
        let record_stages = self.store.record_stages()?;
        let mut batch = ArchiveBatch {
            records: RecordWriter::new(&record_stages)?,
            store: &self.store,
        };
        let mut consumed = 0u64;
        for candidate in candidates.iter() {
            let candidate = candidate?;
            let remaining = limits
                .max_scope_bytes
                .checked_sub(consumed)
                .ok_or_else(|| scope_total_oversized(&candidate.file_name, limits, u64::MAX))?;
            let allowed = limits.max_file_bytes.min(remaining);
            let mut file = open_working_file_candidate(&candidate, self.control())?;
            let before = file.metadata().kio_io(&candidate.path)?;
            let identity = scheduled_source_identity(&file)?;
            if before.len() > allowed {
                return Err(scope_input_oversized(
                    &candidate.file_name,
                    limits,
                    before.len(),
                ));
            }
            let mut stage = self
                .store
                .stage_raw_from_reader(&mut file, allowed)
                .map_err(|e| {
                    e.with_context_field("path", json!(candidate.file_name))
                        .with_context_field("saved", json!(false))
                })?;
            if stage.size_bytes() != before.len()
                || file.metadata().kio_io(&candidate.path)?.len() != stage.size_bytes()
                || scheduled_source_identity(&file)? != identity
            {
                return Err(scope_file_changed(&candidate.file_name));
            }
            consumed = consumed
                .checked_add(stage.size_bytes())
                .ok_or_else(|| scope_total_oversized(&candidate.file_name, limits, u64::MAX))?;
            if let Some(expected) = expected_raw_by_path
                && expected.get(&candidate.file_name).map(String::as_str) != Some(stage.raw_hash())
            {
                return Err(snapshot_authority_changed(
                    "scheduled snapshot inputs changed before publication",
                ));
            }
            batch.records.push(&ArchivedFile {
                name: candidate.file_name,
                size: stage.size_bytes(),
                source: identity,
                stage: stage.saved(),
            })?;
            stage.detach();
        }
        if expected_raw_by_path.is_some_and(|expected| expected.len() != batch.records.len()) {
            return Err(snapshot_authority_changed(
                "scheduled snapshot input set changed before publication",
            ));
        }
        if let Some(expected) = expected_direct_entries
            && self.bound_snapshot_auto_direct_entries()? != *expected
        {
            return Err(snapshot_authority_changed(
                "scheduled snapshot direct entries changed before CAS publication",
            ));
        }
        if scheduled_bound {
            for record in batch.records.iter() {
                let record = record?;
                let candidate = WorkingFileCandidate {
                    path: self.canonical_root.join(&record.name),
                    file_name: record.name.clone(),
                    bound_root: self.bound_root.clone(),
                };
                let mut file = open_working_file_candidate(&candidate, self.control())?;
                let (hash, size) = hash_scope_file(
                    &mut file,
                    &candidate.path,
                    &record.name,
                    limits.max_file_bytes,
                    limits,
                )?;
                let stage = self.store.reopen_saved_stage(&record.stage)?;
                let matches = hash == stage.raw_hash()
                    && size == record.size
                    && file.metadata().kio_io(&candidate.path)?.len() == record.size
                    && scheduled_source_identity(&file)? == record.source;
                let mut stage = stage;
                stage.detach();
                if !matches {
                    return Err(snapshot_authority_changed(
                        "scheduled snapshot source bytes changed before CAS publication",
                    ));
                }
            }
            self.store.validate_bound_layout()?;
            if let Some(policy) = expected_snapshot_policy {
                policy.recheck(
                    self.bound_root.as_deref().expect("bound scope"),
                    self.bound_kio.as_deref().expect("bound kio"),
                )?;
            }
            self.reject_scheduled_bound_purge_state()?;
        }
        let purge = if scheduled_bound {
            None
        } else {
            Some(self.purge_state()?)
        };
        let mut entries = TreeBuilder::new(&record_stages, chunking_config_hash.to_owned());
        self.publish_archive_batch(
            &batch,
            &mut entries,
            normalize_for_path,
            purge_self_targets,
            purge.as_ref(),
        )?;
        Ok(WorkingTree {
            tree: entries.finish()?,
        })
    }

    fn publish_archive_batch(
        &self,
        batch: &ArchiveBatch<'_>,
        entries: &mut TreeBuilder,
        normalize_for_path: &mut dyn FnMut(&str) -> Result<Option<PendingNormalizeRef>>,
        purge_self_targets: &BTreeSet<String>,
        purge: Option<&PurgeState>,
    ) -> Result<()> {
        for record in batch.records.iter() {
            let record = record?;
            let mut stage = self.store.reopen_saved_stage(&record.stage)?;
            if !purge_self_targets.contains(stage.raw_hash()) {
                if let Some(purge) = purge {
                    ensure_raw_publication_allowed(purge, stage.raw_hash())?;
                }
                if purge.is_none() {
                    self.reject_scheduled_marker_targets(std::iter::once(
                        &stage.raw_hash().to_owned(),
                    ))?;
                }
            }
            stage.detach();
        }
        for record in batch.records.iter() {
            let record = record?;
            let mut stage = self.store.reopen_saved_stage(&record.stage)?;
            if purge_self_targets.contains(stage.raw_hash()) {
                stage.detach();
                continue;
            }
            let (hash, _) = self.store.publish_bound_raw_stage(stage).map_err(|e| {
                e.with_context_field("path", json!(record.name))
                    .with_context_field("saved", json!(false))
            })?;
            let mut entry = TreeEntry::raw_file(record.name.clone(), hash)?;
            attach_pending_normalize(&mut entry, &record.name, normalize_for_path(&record.name)?);
            entries.push(entry)?;
        }
        Ok(())
    }

    pub(super) fn working_file_candidates(
        &self,
        include_path: &dyn Fn(&str) -> bool,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
        for_archive: bool,
        limits: ArchiveLimits,
    ) -> Result<WorkingCandidates> {
        let mut candidates = RecordSorter::<String>::new(&self.store.record_stages()?, |a, b| {
            a.as_bytes().cmp(b.as_bytes())
        });
        let mut count = 0usize;
        let mut declared_scope_bytes = 0_u64;
        if let Some(root) = &self.bound_root {
            use cap_primitives::fs as cap_fs;

            let entries = cap_fs::read_base_dir(root).map_err(|error| {
                KioError::io(error.to_string(), self.canonical_root.display().to_string())
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| {
                    KioError::io(error.to_string(), self.canonical_root.display().to_string())
                })?;
                let raw_file_name = entry.file_name();
                let file_name = match raw_file_name.into_string() {
                    Ok(name) => name,
                    Err(file_name) => {
                        eprintln!(
                            "warning: skipping non-UTF-8 file name: {}",
                            self.canonical_root.join(file_name).display()
                        );
                        continue;
                    }
                };
                if file_name == ".kio" || !include_path(&file_name) {
                    continue;
                }
                let path = self.canonical_root.join(&file_name);
                let file_type = entry
                    .file_type()
                    .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
                if file_type.is_dir() {
                    continue;
                }
                if !file_type.is_file() {
                    eprintln!("warning: skipping non-regular file: {}", path.display());
                    continue;
                }
                if for_archive
                    && is_tier_a_secret_name(&file_name)
                    && !explicitly_allowed_tier_a_paths.contains(&file_name)
                {
                    eprintln!("warning: skipping Tier-A secret file: {file_name}");
                    continue;
                }
                if for_archive {
                    if count == TREE_ENTRY_WARNING_THRESHOLD {
                        warn_large_working_tree();
                    }
                    let file =
                        open_bound_scope_file_nofollow(root, &file_name, &path, self.control())?;
                    let metadata = file.metadata().kio_io(&path)?;
                    if metadata.len() > limits.max_file_bytes {
                        return Err(scope_input_oversized(&file_name, limits, metadata.len()));
                    }
                    declared_scope_bytes = declared_scope_bytes
                        .checked_add(metadata.len())
                        .ok_or_else(|| scope_total_oversized(&file_name, limits, u64::MAX))?;
                    if declared_scope_bytes > limits.max_scope_bytes {
                        return Err(scope_total_oversized(
                            &file_name,
                            limits,
                            declared_scope_bytes,
                        ));
                    }
                }
                candidates.push(file_name)?;
                count += 1;
            }
            return Ok(WorkingCandidates {
                names: candidates.finish()?,
                root: self.canonical_root.clone(),
                bound_root: self.bound_root.clone(),
            });
        }
        for entry in fs::read_dir(&self.root).kio_io(&self.root)? {
            let entry = entry.kio_io(&self.root)?;
            let raw_file_name = entry.file_name();
            if raw_file_name == ".kio"
                || raw_file_name
                    .to_str()
                    .is_some_and(|name| !include_path(name))
            {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type().kio_io(&path)?;
            if file_type.is_dir() {
                continue;
            }
            if !file_type.is_file() {
                eprintln!("warning: skipping non-regular file: {}", path.display());
                continue;
            }
            let file_name = match raw_file_name.into_string() {
                Ok(name) => name,
                Err(_) => {
                    eprintln!("warning: skipping non-UTF-8 file name: {}", path.display());
                    continue;
                }
            };
            if for_archive
                && is_tier_a_secret_name(&file_name)
                && !explicitly_allowed_tier_a_paths.contains(&file_name)
            {
                eprintln!("warning: skipping Tier-A secret file: {file_name}");
                continue;
            }
            if for_archive {
                if count == TREE_ENTRY_WARNING_THRESHOLD {
                    warn_large_working_tree();
                }
                let file = open_scope_file_nofollow(&path)?;
                let metadata = file.metadata().kio_io(&path)?;
                if metadata.len() > limits.max_file_bytes {
                    return Err(scope_input_oversized(&file_name, limits, metadata.len()));
                }
                declared_scope_bytes = declared_scope_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| scope_total_oversized(&file_name, limits, u64::MAX))?;
                if declared_scope_bytes > limits.max_scope_bytes {
                    return Err(scope_total_oversized(
                        &file_name,
                        limits,
                        declared_scope_bytes,
                    ));
                }
            }
            candidates.push(file_name)?;
            count += 1;
        }
        Ok(WorkingCandidates {
            names: candidates.finish()?,
            root: self.root.clone(),
            bound_root: None,
        })
    }

    pub fn status(&self) -> Result<StatusReport> {
        self.status_with_deferred(&DiskRecords::empty())
    }

    /// Exclude the sorted deferrals before opening files for status.
    pub fn status_with_deferred(
        &self,
        deferred: &DiskRecords<DeferredInput>,
    ) -> Result<StatusReport> {
        let _inspection = crate::disk_records::StageInspection::begin();
        self.validate()?;
        let limits = ArchiveLimits::default();
        let candidates =
            self.working_file_candidates(&|_| true, &BTreeSet::new(), false, limits)?;
        let current = self
            .read_working_tree(
                candidates,
                &BTreeMap::new(),
                deferred,
                self.effective_chunking_config_hash()?,
                limits,
            )?
            .tree;
        let mut deferred = SortedRecordCursor::new(deferred.iter(), |row| &row.path);
        let (head, head_shallow) = match self.head_tree_state()? {
            HeadTreeState::Unborn => (None, false),
            HeadTreeState::Present(tree) => (Some(tree), false),
            HeadTreeState::Shallow => (None, true),
        };

        let mut current = current.entries.iter();
        let mut head = head.iter().flat_map(|tree| tree.entries.iter());
        let mut new = current.next().transpose()?;
        let mut old = head.next().transpose()?;
        let mut statuses = RecordWriter::new(&self.store.record_stages()?)?;
        while new.is_some() || old.is_some() {
            let order = match (&old, &new) {
                (Some(old), Some(new)) => old.path.cmp(&new.path),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            let (entry, status) = match order {
                std::cmp::Ordering::Less => (
                    std::mem::replace(&mut old, head.next().transpose()?),
                    "deleted",
                ),
                std::cmp::Ordering::Greater => (
                    std::mem::replace(&mut new, current.next().transpose()?),
                    "new",
                ),
                std::cmp::Ordering::Equal => {
                    let previous = std::mem::replace(&mut old, head.next().transpose()?)
                        .expect("matched HEAD entry");
                    let entry = std::mem::replace(&mut new, current.next().transpose()?);
                    let status = if entry
                        .as_ref()
                        .is_some_and(|row| row.raw_hash == previous.raw_hash)
                    {
                        "unchanged"
                    } else {
                        "modified"
                    };
                    (entry, status)
                }
            };
            let entry = entry.expect("status merge has an entry");
            if deferred.at(&entry.path)?.is_some() {
                continue;
            }
            statuses.push(&FileStatus {
                path: is_materializable_direct_child(&entry.path)
                    .then(|| self.root.join(&entry.path)),
                relative_path: entry.path,
                status: (!head_shallow).then(|| status.to_owned()),
                raw_hash: Some(entry.raw_hash),
            })?;
        }
        Ok(StatusReport {
            stage_residues: self.store.inspect_raw_stages()?,
            files: statuses.finish()?,
            head_shallow,
        })
    }
}

fn warn_large_working_tree() {
    eprintln!(
        "warning: scope has more than 10,000 direct files; split into subfolders or use ignore; processing all files"
    );
}

fn scanned_open_failure(root: &File, name: &str, error: KioError) -> Result<DeferredInputReason> {
    use cap_primitives::fs as cap_fs;

    match cap_fs::stat(root, Path::new(name), cap_fs::FollowSymlinks::No) {
        Err(stat) if stat.kind() == std::io::ErrorKind::NotFound => {
            Ok(DeferredInputReason::MissingAfterScan)
        }
        Ok(metadata) if !metadata.is_file() => Ok(DeferredInputReason::ChangedAfterScan),
        _ if error.error_code() == "KIO-E-SCOPE-FILE-CHANGED-001" => {
            Ok(DeferredInputReason::ChangedAfterScan)
        }
        _ => Err(error),
    }
}
