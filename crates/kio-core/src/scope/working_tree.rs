use super::*;

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
        if limits.max_file_bytes > MAX_RAW_OBJECT_BYTES {
            return Err(KioError::invalid_usage(
                "archive max_file_bytes exceeds the raw CAS object limit",
            ));
        }
        let chunking_config_hash = self.effective_chunking_config_hash()?;

        let candidates = self.working_file_candidates(
            excluded_paths,
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
                normalize_by_path,
                limits,
                &BTreeSet::new(),
                None,
                None,
                None,
                &chunking_config_hash,
            );
        }
        let mut entries = Vec::new();
        let mut consumed_scope_bytes = 0_u64;
        for candidate in candidates {
            let mut file = open_working_file_candidate(&candidate, self.control())?;
            let metadata = file.metadata().kio_io(&candidate.path)?;
            let remaining = limits
                .max_scope_bytes
                .checked_sub(consumed_scope_bytes)
                .ok_or_else(|| scope_input_oversized(&candidate.file_name, limits, u64::MAX))?;
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
                .ok_or_else(|| scope_input_oversized(&candidate.file_name, limits, u64::MAX))?;
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
            entries.push(tree_entry);
        }
        Ok(WorkingTree {
            tree: build_tree_with_chunking_config(entries, chunking_config_hash)?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn archive_staged_working_tree(
        &self,
        candidates: Vec<WorkingFileCandidate>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        limits: ArchiveLimits,
        purge_self_targets: &BTreeSet<String>,
        expected_raw_by_path: Option<&BTreeMap<String, String>>,
        expected_direct_entries: Option<&BTreeSet<String>>,
        expected_snapshot_policy: Option<&SnapshotAutoBinding>,
        chunking_config_hash: &str,
    ) -> Result<WorkingTree> {
        let scheduled_bound = expected_raw_by_path.is_some() && self.bound_kio.is_some();
        if scheduled_bound {
            // Recover only canonical private stages through the retained raw
            // descriptor before allocating new ones.  This is bounded and
            // validates every recognized residue; no public descendant path
            // is followed.
            self.store.cleanup_bound_raw_stages()?;
            let mut staged = Vec::with_capacity(candidates.len());
            let mut consumed_scope_bytes = 0_u64;
            for candidate in candidates {
                let remaining = limits
                    .max_scope_bytes
                    .checked_sub(consumed_scope_bytes)
                    .ok_or_else(|| scope_input_oversized(&candidate.file_name, limits, u64::MAX))?;
                let allowed = limits.max_file_bytes.min(remaining);
                let mut source = open_working_file_candidate(&candidate, self.control())?;
                let before = source.metadata().kio_io(&candidate.path)?;
                let identity = scheduled_source_identity(&source)?;
                if before.len() > allowed {
                    return Err(scope_input_oversized(
                        &candidate.file_name,
                        limits,
                        before.len(),
                    ));
                }
                let stage = self.store.stage_raw_from_reader(&mut source, allowed)?;
                let after = source.metadata().kio_io(&candidate.path)?;
                if after.len() != stage.size_bytes()
                    || before.len() != after.len()
                    || identity != scheduled_source_identity(&source)?
                {
                    return Err(scope_file_changed(&candidate.file_name));
                }
                consumed_scope_bytes = consumed_scope_bytes
                    .checked_add(stage.size_bytes())
                    .ok_or_else(|| scope_input_oversized(&candidate.file_name, limits, u64::MAX))?;
                staged.push((candidate, source, before, identity, stage));
            }
            let actual = staged
                .iter()
                .map(|(file, _, _, _, stage)| (file.file_name.clone(), stage.raw_hash().to_owned()))
                .collect::<BTreeMap<_, _>>();
            if Some(&actual) != expected_raw_by_path {
                return Err(snapshot_authority_changed(
                    "scheduled snapshot inputs changed before publication",
                ));
            }
            if self.bound_snapshot_auto_direct_entries()?
                != *expected_direct_entries.expect("scheduled expected entries")
            {
                return Err(snapshot_authority_changed(
                    "scheduled snapshot direct entries changed before CAS publication",
                ));
            }
            // Identity and length are insufficient for an in-place same-size
            // edit. Re-hash each still-open, no-follow source descriptor after
            // the final namespace check and before the first CAS publication.
            for (candidate, source, before, identity, stage) in &mut staged {
                source.seek(SeekFrom::Start(0)).kio_io(&candidate.path)?;
                let (rehash, size) = hash_scope_file(
                    source,
                    &candidate.path,
                    &candidate.file_name,
                    limits.max_file_bytes,
                    limits,
                )?;
                let after = source.metadata().kio_io(&candidate.path)?;
                if rehash != stage.raw_hash()
                    || size != stage.size_bytes()
                    || after.len() != before.len()
                    || scheduled_source_identity(source)? != *identity
                {
                    return Err(snapshot_authority_changed(
                        "scheduled snapshot source bytes changed before CAS publication",
                    ));
                }
            }
            self.store.validate_bound_layout()?;
            if let Some(policy) = expected_snapshot_policy {
                let scope = self.bound_root.as_deref().expect("scheduled bound root");
                let kio = self.bound_kio.as_deref().expect("scheduled bound kio");
                policy.recheck(scope, kio)?;
            }
            self.reject_scheduled_bound_purge_state()?;
            self.reject_scheduled_marker_targets(actual.values())?;
            let mut entries = Vec::with_capacity(staged.len());
            for (candidate, _, _, _, stage) in staged {
                let (published_hash, published_size) = self.store.publish_bound_raw_stage(stage)?;
                if published_size > limits.max_file_bytes {
                    return Err(scope_input_oversized(
                        &candidate.file_name,
                        limits,
                        published_size,
                    ));
                }
                let mut tree_entry =
                    TreeEntry::raw_file(candidate.file_name.clone(), published_hash)?;
                attach_pending_normalize(&mut tree_entry, &candidate.file_name, normalize_by_path);
                tree_entry.validate()?;
                entries.push(tree_entry);
            }
            return Ok(WorkingTree {
                tree: build_tree_with_chunking_config(entries, chunking_config_hash.to_owned())?,
            });
        }
        // The caller holds `.kio/.lock`, so no live Kio writer can own an
        // `.ingest-*` leaf. Remove crash-orphaned raw bytes before creating any
        // new staging file; otherwise a tombstoned re-ingest killed before its
        // authorization check could remain in KIO-managed storage indefinitely.
        // Scheduled publication has retained object descriptors but must not
        // follow the ordinary path-based crash-temp scavenger: a same-UID
        // descendant replacement must be a fail-closed no-op, never a cleanup
        // of the replacement target. Its bound ObjectStore owns raw CAS writes.
        if !scheduled_bound {
            cleanup_orphan_raw_ingest_temps(&self.kio_dir)?;
        }
        let mut staged = Vec::with_capacity(candidates.len());
        let mut consumed_scope_bytes = 0_u64;
        for candidate in candidates {
            let remaining = limits
                .max_scope_bytes
                .checked_sub(consumed_scope_bytes)
                .ok_or_else(|| scope_input_oversized(&candidate.file_name, limits, u64::MAX))?;
            let allowed = limits.max_file_bytes.min(remaining);
            let staged_file =
                stage_scope_file(&self.kio_dir, candidate, allowed, limits, self.control())?;
            consumed_scope_bytes = consumed_scope_bytes
                .checked_add(staged_file.size_bytes)
                .ok_or_else(|| {
                    scope_input_oversized(&staged_file.candidate.file_name, limits, u64::MAX)
                })?;
            staged.push(staged_file);
        }

        // A scheduled decision is authorized by a descriptor-bound raw-hash
        // preview.  Compare that exact path/hash set after every source has
        // been copied into private staging files but before the first CAS
        // publication.  An editor changing, adding, deleting, or renaming a
        // file in the final observation-to-publication window therefore
        // causes a retryable failure instead of a commit with a stale report.
        if let Some(expected) = expected_raw_by_path {
            let actual = staged
                .iter()
                .map(|file| (file.candidate.file_name.clone(), file.raw_hash.clone()))
                .collect::<BTreeMap<_, _>>();
            if &actual != expected {
                drop(staged);
                if !scheduled_bound {
                    let raw_dir = self.kio_dir.join("objects/raw");
                    crate::purge::sync_directory(&raw_dir).kio_io(&raw_dir)?;
                }
                return Err(KioError::new(
                    "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001",
                    "scheduled snapshot inputs changed before publication",
                    json!({}),
                    ExitCode::PartialFailure,
                ));
            }
        }
        if let Some(expected) = expected_direct_entries
            && self.bound_snapshot_auto_direct_entries()? != *expected
        {
            drop(staged);
            if !scheduled_bound {
                let raw_dir = self.kio_dir.join("objects/raw");
                crate::purge::sync_directory(&raw_dir).kio_io(&raw_dir)?;
            }
            return Err(snapshot_authority_changed(
                "scheduled snapshot direct entries changed before CAS publication",
            ));
        }

        // Discover every candidate barrier before the first raw CAS publication.
        // Thus a later path carrying an incomplete-purge identity cannot leave
        // earlier, otherwise-valid candidates partially archived. A public
        // tombstone or fsck-only erase receipt no longer blocks publication
        // (U19/LC22: the permanent re-ingest rejection is reversed into a
        // resurrection flow) — only an active purge journal barrier
        // (incomplete purge in progress) still gates ingest here.
        //
        // P2-A finding / 05 §3.5 L741 ("working tree の原本には触れない"):
        // `purge_self_targets` is non-empty ONLY when this call is itself
        // `Repository::purged_snapshot`'s own working-tree rebuild (the sole
        // producer — see its call site in `snapshot_with_type`), and its
        // members are exactly THIS journal's own `target_raw_hashes`. A
        // single `.kio` scope holds at most one active purge journal at a
        // time (the store lock this method's caller already holds
        // serializes purge with every other write), so any raw_hash this
        // barrier would block during a purge's own snapshot is necessarily
        // this same purge's own journal — never a foreign one. Purge
        // deliberately never deletes the working-tree original (05 §3.5),
        // so a residual copy of a purge target's exact bytes legitimately
        // reappears here on the SAME pass that is finalizing that target's
        // removal; treating it as a blocked *ingest* would wrongly fail the
        // purge's own completing commit. It is excluded below instead —
        // neither re-published to CAS nor added to the resulting tree — so
        // `commit_type=purged`'s tree never carries the entry it is purging
        // (matching `verify_purged_commit`'s postcondition), while every
        // OTHER (non-self) raw_hash still gets the barrier's full
        // protection. Re-ingestion of a residual original is deferred to
        // the next ordinary `kio index` (05 §3.5 L743), once no journal
        // remains to block it.
        if !scheduled_bound {
            let purge = self.purge_state()?;
            for file in &staged {
                if purge_self_targets.contains(&file.raw_hash) {
                    continue;
                }
                ensure_raw_publication_allowed(&purge, &file.raw_hash)?;
            }
        }

        let mut entries = Vec::with_capacity(staged.len());
        for mut staged_file in staged {
            if purge_self_targets.contains(&staged_file.raw_hash) {
                continue;
            }
            staged_file
                .file
                .seek(SeekFrom::Start(0))
                .kio_io(&staged_file.temp_path)?;
            let (published_hash, published_size) = self
                .store
                .write_raw_reader(&mut staged_file.file, staged_file.size_bytes)?;
            if published_hash != staged_file.raw_hash || published_size != staged_file.size_bytes {
                return Err(scope_file_changed(&staged_file.candidate.file_name));
            }
            let mut tree_entry =
                TreeEntry::raw_file(staged_file.candidate.file_name.clone(), published_hash)?;
            attach_pending_normalize(
                &mut tree_entry,
                &staged_file.candidate.file_name,
                normalize_by_path,
            );
            tree_entry.validate()?;
            entries.push(tree_entry);
        }
        Ok(WorkingTree {
            tree: build_tree_with_chunking_config(entries, chunking_config_hash.to_owned())?,
        })
    }

    pub(super) fn working_file_candidates(
        &self,
        excluded_paths: &BTreeSet<String>,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
        enforce_tier_a: bool,
        limits: ArchiveLimits,
    ) -> Result<Vec<WorkingFileCandidate>> {
        let mut candidates = Vec::new();
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
                if file_name == ".kio" {
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
                if excluded_paths.contains(&file_name) {
                    continue;
                }
                if enforce_tier_a
                    && is_tier_a_secret_name(&file_name)
                    && !explicitly_allowed_tier_a_paths.contains(&file_name)
                {
                    eprintln!("warning: skipping Tier-A secret file: {file_name}");
                    continue;
                }
                if candidates.len() == MAX_TREE_ENTRIES {
                    return Err(scope_tree_entries_oversized(
                        candidates.len().saturating_add(1),
                    ));
                }
                let file = open_bound_scope_file_nofollow(root, &file_name, &path, self.control())?;
                let metadata = file.metadata().kio_io(&path)?;
                if metadata.len() > limits.max_file_bytes {
                    return Err(scope_input_oversized(&file_name, limits, metadata.len()));
                }
                declared_scope_bytes = declared_scope_bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| scope_input_oversized(&file_name, limits, u64::MAX))?;
                if declared_scope_bytes > limits.max_scope_bytes {
                    return Err(scope_input_oversized(
                        &file_name,
                        limits,
                        declared_scope_bytes,
                    ));
                }
                candidates.push(WorkingFileCandidate {
                    path,
                    file_name,
                    bound_root: Some(Arc::clone(root)),
                });
            }
            return Ok(candidates);
        }
        for entry in fs::read_dir(&self.root).kio_io(&self.root)? {
            let entry = entry.kio_io(&self.root)?;
            if entry.file_name() == ".kio" {
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
            let file_name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => {
                    eprintln!("warning: skipping non-UTF-8 file name: {}", path.display());
                    continue;
                }
            };
            if excluded_paths.contains(&file_name) {
                continue;
            }
            if enforce_tier_a
                && is_tier_a_secret_name(&file_name)
                && !explicitly_allowed_tier_a_paths.contains(&file_name)
            {
                eprintln!("warning: skipping Tier-A secret file: {file_name}");
                continue;
            }
            if candidates.len() == MAX_TREE_ENTRIES {
                return Err(scope_tree_entries_oversized(
                    candidates.len().saturating_add(1),
                ));
            }
            let file = open_scope_file_nofollow(&path)?;
            let metadata = file.metadata().kio_io(&path)?;
            if metadata.len() > limits.max_file_bytes {
                return Err(scope_input_oversized(&file_name, limits, metadata.len()));
            }
            declared_scope_bytes = declared_scope_bytes
                .checked_add(metadata.len())
                .ok_or_else(|| scope_input_oversized(&file_name, limits, u64::MAX))?;
            if declared_scope_bytes > limits.max_scope_bytes {
                return Err(scope_input_oversized(
                    &file_name,
                    limits,
                    declared_scope_bytes,
                ));
            }
            candidates.push(WorkingFileCandidate {
                path,
                file_name,
                bound_root: None,
            });
        }
        Ok(candidates)
    }

    pub fn status(&self) -> Result<StatusReport> {
        self.validate()?;
        let current = self.build_working_tree(false)?.tree;
        let current_map = tree_map(&current);
        // R15-4: a shallow HEAD (tree object discarded) must NOT brick a pure read.
        // Degrade to listing the current files without a classification instead of
        // propagating the raw KIO-E-STORE-NOT-FOUND-001 from `read_tree`.
        let (head_map, head_shallow) = match self.head_tree_state()? {
            HeadTreeState::Unborn => (BTreeMap::new(), false),
            HeadTreeState::Present(tree) => (tree_map(&tree), false),
            HeadTreeState::Shallow => (BTreeMap::new(), true),
        };

        let mut paths = BTreeSet::new();
        paths.extend(current_map.keys().cloned());
        paths.extend(head_map.keys().cloned());

        let mut statuses = Vec::new();
        for path in paths {
            // Omit the tree-derived classification when HEAD is shallow — the prior
            // tree needed to compute it is gone.
            let status = if head_shallow {
                if !current_map.contains_key(&path) {
                    continue;
                }
                None
            } else {
                Some(
                    match (head_map.get(&path), current_map.get(&path)) {
                        (None, Some(_)) => "new",
                        (Some(old), Some(new)) if old == new => "unchanged",
                        (Some(_), Some(_)) => "modified",
                        (Some(_), None) => "deleted",
                        (None, None) => continue,
                    }
                    .to_owned(),
                )
            };
            statuses.push(FileStatus {
                path: is_materializable_direct_child(&path).then(|| self.root.join(&path)),
                relative_path: path.clone(),
                status,
                raw_hash: current_map
                    .get(&path)
                    .or_else(|| head_map.get(&path))
                    .cloned(),
            });
        }
        Ok(StatusReport {
            files: statuses,
            head_shallow,
        })
    }
}
