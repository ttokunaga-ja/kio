use super::*;

impl Repository {
    pub fn snapshot(
        &self,
        message: Option<&str>,
        fixed_now: Option<&str>,
    ) -> Result<SnapshotOutcome> {
        self.snapshot_filtered(message, fixed_now, &BTreeSet::new())
    }

    /// Manual snapshot that honors preview exclusions and reclassifies Tier-A
    /// names at the final read boundary. The policy-aware variant preserves an
    /// explicit local-only Tier-A unignore from the same preview.
    pub fn snapshot_filtered(
        &self,
        message: Option<&str>,
        fixed_now: Option<&str>,
        excluded_paths: &BTreeSet<String>,
    ) -> Result<SnapshotOutcome> {
        self.snapshot_filtered_with_policy(message, fixed_now, excluded_paths, &BTreeSet::new())
    }

    pub fn snapshot_filtered_with_policy(
        &self,
        message: Option<&str>,
        fixed_now: Option<&str>,
        excluded_paths: &BTreeSet<String>,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
    ) -> Result<SnapshotOutcome> {
        self.snapshot_with_type(
            message,
            fixed_now,
            CommitType::Manual,
            excluded_paths,
            &BTreeMap::new(),
            explicitly_allowed_tier_a_paths,
            false,
            &[],
            true,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    pub fn auto_snapshot_with_normalize(
        &self,
        message: Option<&str>,
        fixed_now: Option<&str>,
        excluded_paths: &BTreeSet<String>,
        normalize_by_path: &BTreeMap<String, NormalizeRef>,
    ) -> Result<SnapshotOutcome> {
        if !normalize_by_path.is_empty() {
            return Err(KioError::invalid_usage(
                "normalization metadata must include its expected raw hash",
            ));
        }
        self.auto_snapshot_with_bound_normalize(
            message,
            fixed_now,
            excluded_paths,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
        )
    }

    /// The optional read-only guard runs after immutable preparation, immediately
    /// before publication (or noop success). It must not mutate scope state.
    pub fn auto_snapshot_with_bound_normalize(
        &self,
        message: Option<&str>,
        fixed_now: Option<&str>,
        excluded_paths: &BTreeSet<String>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
        publication_guard: Option<&dyn Fn() -> Result<()>>,
    ) -> Result<SnapshotOutcome> {
        self.snapshot_with_type(
            message,
            fixed_now,
            CommitType::Auto,
            excluded_paths,
            normalize_by_path,
            explicitly_allowed_tier_a_paths,
            false,
            &[],
            true,
            None,
            None,
            None,
            None,
            None,
            publication_guard,
        )
    }

    pub(super) fn bound_snapshot_auto_direct_entries(&self) -> Result<BTreeSet<String>> {
        let root = self.bound_root.as_deref().ok_or_else(|| {
            KioError::invalid_usage(
                "scheduled snapshot publication requires a retained scope capability",
            )
        })?;
        crate::gc::validated_snapshot_auto_direct_entries(root)
    }

    /// Validate the scheduler's immutable writer inputs through the retained
    /// `.kio` directory.  This deliberately has stricter semantics than the
    /// ordinary open path: an empty HEAD with a populated branch ref is
    /// corruption here, not an opportunity for automatic repair.
    fn validate_scheduled_auto_prerequisites(&self) -> Result<ScheduledSnapshotAuthority> {
        if self.bound_kio.is_none() {
            return Err(KioError::invalid_usage(
                "scheduled snapshot publication requires a retained .kio capability",
            ));
        }
        if self.bound_root.is_none() {
            return Err(KioError::invalid_usage(
                "scheduled snapshot publication requires a retained scope capability",
            ));
        }
        let authority = self.capture_scheduled_snapshot_authority()?;
        if let Some(head) = parse_head_ref(&authority.head)? {
            // Both objects are CAS-bound in this repository.  Do not permit a
            // scheduled writer to extend a shallow/corrupt history.
            let commit = self.read_commit(&head)?;
            self.read_tree(&commit.tree)?;
        }
        self.reject_scheduled_bound_purge_state()?;
        Ok(authority)
    }

    /// Capture every mutable metadata leaf that selects the scheduled
    /// snapshot's parent and tool identity.  Values alone are not enough: a
    /// same-byte inode replacement is an authority change too.
    fn capture_scheduled_snapshot_authority(&self) -> Result<ScheduledSnapshotAuthority> {
        let kio = self.bound_kio.as_deref().ok_or_else(|| {
            KioError::invalid_usage("scheduled snapshot requires retained .kio capability")
        })?;
        let (head, head_observation) =
            read_bound_regular_text_observed_at(kio, "HEAD", MAX_BOUND_CONFIG_BYTES)?;
        wait_at_bound_snapshot_auto_barrier(self.authority_capture_ready());
        let (tool_lock, tool_lock_observation) =
            read_bound_regular_text_observed_at(kio, "tool-lock.json", MAX_BOUND_CONFIG_BYTES)?;
        let tool_lock_value: Value = serde_json::from_str(&tool_lock)
            .map_err(|error| KioError::schema(error.to_string()))?;
        canonical_tool_lock_value(&tool_lock_value)?;
        Ok(ScheduledSnapshotAuthority {
            head,
            tool_lock,
            head_observation,
            tool_lock_observation,
        })
    }

    fn recheck_scheduled_snapshot_authority(
        &self,
        expected: &ScheduledSnapshotAuthority,
    ) -> Result<()> {
        let actual = self.capture_scheduled_snapshot_authority().map_err(|_| {
            snapshot_authority_changed("scheduled snapshot metadata authority changed")
        })?;
        if actual != *expected {
            return Err(snapshot_authority_changed(
                "scheduled snapshot HEAD or tool lock changed",
            ));
        }
        Ok(())
    }

    fn recheck_scheduled_published_authority(
        &self,
        expected: &ScheduledSnapshotAuthority,
        published: &ScheduledSnapshotAuthority,
        commit_hash: &str,
    ) -> Result<()> {
        let actual = self.capture_scheduled_snapshot_authority().map_err(|_| {
            snapshot_authority_changed("scheduled snapshot metadata authority changed")
        })?;
        if actual.head.trim() != commit_hash
            || actual.head_observation != published.head_observation
            || actual.tool_lock != expected.tool_lock
            || actual.tool_lock_observation != expected.tool_lock_observation
        {
            return Err(snapshot_authority_changed(
                "scheduled snapshot HEAD or tool lock changed after publication",
            ));
        }
        Ok(())
    }

    pub(super) fn reject_scheduled_bound_purge_state(&self) -> Result<()> {
        use cap_primitives::fs as cap_fs;

        let kio = self.bound_kio.as_deref().expect("validated above");
        // A scheduled writer never drives purge recovery or marker retirement.
        // The parent directory may legitimately remain after completed recovery
        // because it contains the monotonic epoch; only the active journal is
        // the writer barrier.
        match cap_fs::open_dir_nofollow(kio, Path::new("purge")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(KioError::io(error.to_string(), "purge")),
            Ok(purge) => match cap_fs::stat(
                &purge,
                Path::new("in-progress.json"),
                cap_fs::FollowSymlinks::No,
            ) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(metadata) if metadata.is_file() => {
                    return Err(KioError::new(
                        "KIO-E-PURGE-INCOMPLETE-001",
                        "scheduled snapshot is blocked by an active purge journal",
                        json!({ "component": "snapshot_auto" }),
                        ExitCode::PartialFailure,
                    ));
                }
                Ok(_) => {
                    return Err(unsafe_store_error(
                        Path::new("purge/in-progress.json"),
                        "purge journal is not a regular file",
                    ));
                }
                Err(error) => {
                    return Err(KioError::io(error.to_string(), "purge/in-progress.json"));
                }
            },
        }
        Ok(())
    }

    pub(super) fn reject_scheduled_marker_targets<'a>(
        &self,
        raw_hashes: impl IntoIterator<Item = &'a String>,
    ) -> Result<()> {
        let kio = self.bound_kio.as_deref().ok_or_else(|| {
            KioError::invalid_usage("scheduled snapshot requires retained .kio capability")
        })?;
        for raw_hash in raw_hashes {
            for namespace in ["tombstones", "purge/erase-receipts"] {
                if bound_marker_exists(kio, namespace, raw_hash)? {
                    return Err(KioError::new(
                        "KIO-E-PURGE-INCOMPLETE-001",
                        "scheduled snapshot refuses a raw identity with an existing purge marker",
                        json!({ "component": "snapshot_auto" }),
                        ExitCode::PartialFailure,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Scheduler-only auto snapshot. Historical normalization is carried only
    /// through a raw-hash-bound reference, never from a mutable cache.
    #[allow(clippy::too_many_arguments)]
    pub fn scheduled_auto_snapshot(
        &self,
        fixed_now: &str,
        excluded_paths: &BTreeSet<String>,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
        expected_raw_by_path: &BTreeMap<String, String>,
        expected_direct_entries: &BTreeSet<String>,
        expected_snapshot_policy: &SnapshotAutoBinding,
        before_ref_publication: &mut dyn FnMut() -> Result<SnapshotAutoStateBinding>,
    ) -> Result<SnapshotOutcome> {
        // Freeze HEAD and its normalization references in the same writer
        // critical section as the eventual snapshot. Otherwise a concurrent
        // promotion on unchanged raw bytes could be replaced by the older
        // reference captured just before the nested snapshot lock.
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        // This entrypoint is only valid for the retained-descriptor repository
        // constructed by the scheduler handoff.  In particular, do not apply
        // the ordinary empty-HEAD recovery here: a scheduled writer must never
        // turn a replaceable mutable ref into a new authority for publication.
        let scheduled_authority = self.validate_scheduled_auto_prerequisites()?;
        self.store.validate_bound_layout()?;
        expected_snapshot_policy.recheck(
            self.bound_root.as_deref().expect("scheduled bound root"),
            self.bound_kio.as_deref().expect("scheduled bound kio"),
        )?;
        self.reject_scheduled_marker_targets(expected_raw_by_path.values())?;
        if self.bound_snapshot_auto_direct_entries()? != *expected_direct_entries {
            return Err(snapshot_authority_changed(
                "scheduled snapshot direct entries changed before publication",
            ));
        }
        let mut normalize_by_path = BTreeMap::new();
        if !scheduled_authority.head.trim().is_empty() {
            let head = scheduled_authority.head.trim();
            let tree = self.read_tree(&self.read_commit(head)?.tree)?;
            for entry in tree.entries {
                if let Some(normalize) = entry.normalize {
                    normalize_by_path.insert(
                        entry.path.clone(),
                        PendingNormalizeRef {
                            expected_raw_hash: entry.raw_hash,
                            normalize,
                        },
                    );
                }
            }
        }
        self.snapshot_with_type(
            Some("scheduled auto snapshot"),
            Some(fixed_now),
            CommitType::Auto,
            excluded_paths,
            &normalize_by_path,
            explicitly_allowed_tier_a_paths,
            false,
            &[],
            true,
            Some(expected_raw_by_path),
            Some(expected_direct_entries),
            Some(expected_snapshot_policy),
            Some(&scheduled_authority),
            Some(before_ref_publication),
            None,
        )
    }

    /// Promote verified normalized identities on existing HEAD entries without
    /// snapshotting unrelated working-tree edits.
    ///
    /// The caller supplies content-bound references after validating the online
    /// result and current policy.  This boundary independently hashes the current
    /// working tree while holding the store lock and applies a reference only when
    /// both HEAD and the current file still equal `expected_raw_hash`.  A changed,
    /// deleted, or newly-created path is ignored.  Every unrelated HEAD entry is
    /// retained byte-for-byte, so one accepted online task cannot accidentally
    /// commit a sibling edit that happened while the task was deferred.
    pub fn promote_normalize_refs(
        &self,
        message: Option<&str>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
    ) -> Result<SnapshotOutcome> {
        self.promote_normalize_refs_with_tool_lock_hash(message, normalize_by_path, None)
    }

    /// Promotion variant used by a caller that has durably staged the next
    /// tool-lock and therefore needs the commit to attest that staged identity
    /// before the mutable live projection is published.
    pub fn promote_normalize_refs_with_staged_tool_lock(
        &self,
        message: Option<&str>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        tool_lock_hash: &str,
    ) -> Result<SnapshotOutcome> {
        if !is_hash(tool_lock_hash) {
            return Err(KioError::schema(
                "staged promotion tool_lock_hash must be sha256 lowercase hex",
            ));
        }
        self.store
            .inspect_content_object(ContentObjectKind::Toollock, tool_lock_hash)
            .map_err(|_| {
                KioError::schema(
                    "staged promotion tool_lock_hash must name a published immutable tool-lock",
                )
            })?;
        self.promote_normalize_refs_with_tool_lock_hash(
            message,
            normalize_by_path,
            Some(tool_lock_hash),
        )
    }

    fn promote_normalize_refs_with_tool_lock_hash(
        &self,
        message: Option<&str>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        staged_tool_lock_hash: Option<&str>,
    ) -> Result<SnapshotOutcome> {
        self.validate()?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        let head_hash = self
            .head_commit_hash()?
            .ok_or_else(|| KioError::invalid_usage("cannot promote in an unborn scope"))?;
        let head_commit = self.read_commit(&head_hash).map_err(|error| {
            if is_store_not_found(&error) {
                KioError::commit_shallow(
                    "HEAD commit object is missing; online output was not promoted",
                    head_hash.clone(),
                )
            } else {
                error
            }
        })?;
        let prior_tree = match self.read_tree(&head_commit.tree) {
            Ok(tree) => tree,
            Err(error) if is_store_not_found(&error) => {
                crate::gc::GcSweepSession::bind_repository(self)?
                    .validate_final_shallow_tree(&head_hash, &head_commit.tree)?;
                return Err(KioError::commit_shallow(
                    "HEAD tree object is missing; online output was not promoted",
                    head_hash.clone(),
                ));
            }
            Err(error) => return Err(error),
        };
        let current = self.build_working_tree(false)?.tree;
        let current_raw = tree_map(&current);
        let purge = self.purge_state()?;
        let mut promoted_tree = prior_tree.clone();
        let mut changed = false;
        for entry in &mut promoted_tree.entries {
            let Some(pending) = normalize_by_path.get(&entry.path) else {
                continue;
            };
            let exact_current = current_raw
                .get(&entry.path)
                .is_some_and(|raw_hash| raw_hash == &pending.expected_raw_hash);
            if !exact_current || entry.raw_hash != pending.expected_raw_hash {
                continue;
            }
            ensure_raw_publication_allowed(&purge, &pending.expected_raw_hash)?;
            if entry.normalize.as_ref() != Some(&pending.normalize) {
                entry.normalize = Some(pending.normalize.clone());
                changed = true;
            }
        }
        promoted_tree.validate()?;
        let stats = commit_stats(Some(&prior_tree), &promoted_tree);
        if !changed {
            return Ok(SnapshotOutcome {
                noop: true,
                message: "promotion noop: bindings unchanged".to_owned(),
                tree_hash: head_commit.tree,
                commit_hash: None,
                commit: None,
                stats,
            });
        }

        let tree_value = serde_json::to_value(&promoted_tree)
            .map_err(|error| KioError::schema(error.to_string()))?;
        let (tree_hash, _) = self.store.write_json(ObjectKind::Tree, &tree_value)?;
        let created_at = self.now_utc_seconds();
        let commit = CommitObject::new(
            tree_hash.clone(),
            Some(head_hash.clone()),
            created_at.clone(),
            message
                .map(str::to_owned)
                .unwrap_or_else(|| format!("online Markdownize promotion at {created_at}")),
            match staged_tool_lock_hash {
                Some(hash) => hash.to_owned(),
                None => self.publish_tool_lock()?,
            },
            stats.clone(),
            CommitType::Auto,
        )?;
        let commit_value =
            serde_json::to_value(&commit).map_err(|error| KioError::schema(error.to_string()))?;
        let (commit_hash, _) = self.store.write_json(ObjectKind::Commit, &commit_value)?;
        let manifest = self.manifest_value(&promoted_tree, Some(&prior_tree))?;
        self.publish_with_journal(Some(&head_hash), &commit_hash, &commit, manifest)?;
        Ok(SnapshotOutcome {
            noop: false,
            message: "online Markdownize promotion created".to_owned(),
            tree_hash,
            commit_hash: Some(commit_hash),
            commit: Some(commit),
            stats,
        })
    }

    /// Capture the purge-time working tree and force exactly one protected
    /// `commit_type=purged` child even when the tree equals HEAD. Unchanged
    /// files retain their existing normalize bindings; changed/new files do not
    /// inherit stale normalization metadata.
    ///
    /// `publish_ref=false` (05-runtime.md §3.5's `prepared` phase, LC48)
    /// computes and durably CAS-writes the commit object — fixing its hash as
    /// `planned_commit` — without publishing `HEAD` or
    /// running the resurrection-retire scan; the purge orchestration (this
    /// journal's `prepared` step) uses this to fix `planned_commit` before any
    /// tombstone/erase-receipt is durable. `publish_ref=true` (the journal's
    /// `committed` phase) re-derives the identical commit (deterministic,
    /// content-addressed — the store lock has been held across the whole
    /// operation so the working tree cannot have changed) and publishes it for
    /// real.
    pub fn purged_snapshot(
        &self,
        reason: &str,
        fixed_now: Option<&str>,
        purged_raws: &[String],
        publish_ref: bool,
    ) -> Result<SnapshotOutcome> {
        self.validate()?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        let head = self
            .head_commit_hash()?
            .ok_or_else(|| KioError::invalid_usage("cannot purge an unborn scope"))?;
        let head_commit = self.read_commit(&head)?;
        let head_tree = self.read_tree(&head_commit.tree)?;
        let normalize_by_path = head_tree
            .entries
            .iter()
            .filter_map(|entry| {
                entry.normalize.clone().map(|normalize| {
                    (
                        entry.path.clone(),
                        PendingNormalizeRef {
                            expected_raw_hash: entry.raw_hash.clone(),
                            normalize,
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        self.snapshot_with_type(
            Some(reason),
            fixed_now,
            CommitType::Purged,
            &BTreeSet::new(),
            &normalize_by_path,
            &BTreeSet::new(),
            true,
            purged_raws,
            publish_ref,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// Record a successful object repair without rescanning or changing the
    /// snapshot tree. The caller holds the store lock while restoring verified
    /// bytes; this method is reentrant and advances refs only after the repaired
    /// commit object is durable.
    pub fn record_repaired_commit(&self, message: Option<&str>) -> Result<String> {
        self.validate()?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        let head = self
            .head_commit_hash()?
            .ok_or_else(|| KioError::invalid_usage("cannot record repair in an unborn scope"))?;
        let head_commit = self.read_commit(&head)?;
        // The tree itself must still be verified before a new commit refers to it.
        self.read_tree(&head_commit.tree)?;
        let created_at = self.now_utc_seconds();
        let commit = CommitObject::new(
            head_commit.tree.clone(),
            Some(head.clone()),
            created_at.clone(),
            message
                .map(str::to_owned)
                .unwrap_or_else(|| format!("object repair at {created_at}")),
            self.publish_tool_lock()?,
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            CommitType::Repaired,
        )?;
        let commit_value =
            serde_json::to_value(&commit).map_err(|error| KioError::schema(error.to_string()))?;
        let (commit_hash, _) = self.store.write_json(ObjectKind::Commit, &commit_value)?;
        let tree = self.read_tree(&head_commit.tree)?;
        let manifest = self.manifest_value(&tree, Some(&tree))?;
        self.publish_with_journal(Some(&head), &commit_hash, &commit, manifest)?;
        Ok(commit_hash)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn snapshot_with_type(
        &self,
        message: Option<&str>,
        fixed_now: Option<&str>,
        commit_type: CommitType,
        excluded_paths: &BTreeSet<String>,
        normalize_by_path: &BTreeMap<String, PendingNormalizeRef>,
        explicitly_allowed_tier_a_paths: &BTreeSet<String>,
        force_commit: bool,
        purged_raws: &[String],
        publish_ref: bool,
        expected_raw_by_path: Option<&BTreeMap<String, String>>,
        expected_direct_entries: Option<&BTreeSet<String>>,
        expected_snapshot_policy: Option<&SnapshotAutoBinding>,
        expected_scheduled_authority: Option<&ScheduledSnapshotAuthority>,
        before_ref_publication: Option<&mut dyn FnMut() -> Result<SnapshotAutoStateBinding>>,
        publication_guard: Option<&dyn Fn() -> Result<()>>,
    ) -> Result<SnapshotOutcome> {
        // A retained repository is also used by the existing child-index
        // subprocess.  Bound descriptors plus `commit_type=auto` therefore do
        // not identify a scheduled publication by themselves.  The scheduler
        // is the only caller that supplies its validated authority binding.
        let scheduled_bound = expected_scheduled_authority.is_some();
        self.validate()?;
        let _lock = self.lock_store_in_session(&self.lock_session)?;
        maybe_hold_lock_for_tests(self.hold_lock_ready());
        let chunking_config_hash = self.effective_chunking_config_hash()?;

        // Validate the base before archiving any working-tree bytes.  A normal
        // snapshot needs its parent tree to compute stats and write a coherent
        // manifest, so a missing tree is only acceptable when it is a *final*
        // receipt-backed shallow boundary.  Even then a new snapshot cannot
        // extend it.  Doing this before `archive_staged_working_tree` is
        // important: that routine writes raw/tree CAS objects as it captures
        // the working tree, and a rejected shallow base must not leave those
        // newly published objects behind.
        let head_hash = if let Some(authority) = expected_scheduled_authority {
            let head = authority.head.trim();
            if head.is_empty() {
                None
            } else {
                Some(head.to_owned())
            }
        } else {
            self.head_commit_hash()?
        };
        let head_commit = match head_hash.as_deref() {
            None => None,
            Some(hash) => match self.read_commit(hash) {
                Ok(commit) => Some(commit),
                Err(error) if is_store_not_found(&error) => {
                    return Err(KioError::commit_shallow(
                        "HEAD commit object is missing (shallow: discarded / corrupt); \
                         restore the commit object or re-create the scope before snapshotting",
                        hash.to_owned(),
                    ));
                }
                Err(error) => return Err(error),
            },
        };
        let head_tree_hash = head_commit.as_ref().map(|commit| commit.tree.clone());
        // Snapshot the prior HEAD tree before writing the candidate working
        // tree.  `validate_final_shallow_tree` distinguishes a canonical
        // final shallow boundary from a receiptless, mismatched, manual, or
        // currently-referenced missing tree (all corruption).  A valid final
        // boundary still cannot be used as a snapshot parent.
        let prior_tree = match head_tree_hash.as_deref() {
            None => None,
            Some(hash) => match self.read_tree(hash) {
                Ok(tree) => Some(tree),
                Err(error) if is_store_not_found(&error) => {
                    crate::gc::GcSweepSession::bind_repository(self)?.validate_final_shallow_tree(
                        head_hash.as_deref().unwrap_or_default(),
                        hash,
                    )?;
                    return Err(KioError::commit_shallow(
                        "HEAD commit is shallow (tree object discarded); \
                         restore the tree object or re-create the scope before snapshotting",
                        head_hash.clone().unwrap_or_default(),
                    ));
                }
                Err(error) => return Err(error),
            },
        };

        // P2-A finding (05 §3.5 L741, `archive_staged_working_tree`'s doc
        // comment): a `commit_type=purged` snapshot's own `purged_raws` are
        // self-owned targets, not a foreign barrier to respect — bypass the
        // public `build_working_tree_with_bound_normalize_and_limits`
        // wrapper (which always applies the barrier unconditionally) and
        // call the two lower-level steps directly so `purge_self_targets`
        // can be threaded through. Every other `commit_type` gets an empty
        // set here and this is exactly equivalent to the wrapper call it
        // replaces.
        let purge_self_targets: BTreeSet<String> = if commit_type == CommitType::Purged {
            purged_raws.iter().cloned().collect()
        } else {
            BTreeSet::new()
        };
        let working_candidates = self.working_file_candidates(
            excluded_paths,
            explicitly_allowed_tier_a_paths,
            true,
            ArchiveLimits::default(),
        )?;
        let working = {
            let _archive_lock = self.lock_store_in_session(&self.lock_session)?;
            self.archive_staged_working_tree(
                working_candidates,
                normalize_by_path,
                ArchiveLimits::default(),
                &purge_self_targets,
                expected_raw_by_path,
                expected_direct_entries,
                expected_snapshot_policy,
                &chunking_config_hash,
            )?
        }
        .tree;

        // U19/LC22-LC26: a raw_hash that is both (a) present in the tree we are
        // about to commit and (b) currently the target of an *active*
        // tombstone/erase-receipt is a resurrection candidate — raw CAS bytes
        // were just (re)published for it above (`build_working_tree_...`'s
        // `archive_staged_working_tree`, content-addressed and therefore
        // idempotent whether or not the object already existed). This purge
        // commit's own snapshot (`commit_type == Purged`) is excluded: its
        // own targets never even reach `working.entries` any more (the
        // `purge_self_targets` exclusion just above), so there is nothing of
        // this purge's own making to treat as resurrected, and no tombstone
        // for it exists yet at this point in the purge orchestration (the
        // `prepared`-phase dry run runs before either is durable) — the guard
        // stays defensive (keeps a mid-purge re-run of this method, e.g.
        // re-purge, from ever retiring the marker it is itself about to
        // create).
        let resurrection_candidates = if scheduled_bound || commit_type == CommitType::Purged {
            BTreeSet::new()
        } else {
            let purge = self.purge_state()?;
            let mut candidates = BTreeSet::new();
            for entry in &working.entries {
                let tombstoned = purge
                    .read_tombstone(&entry.raw_hash)?
                    .is_some_and(|record| record.is_active());
                let erased = purge
                    .read_erase_receipt(&entry.raw_hash)?
                    .is_some_and(|receipt| receipt.is_active());
                if tombstoned || erased {
                    candidates.insert(entry.raw_hash.clone());
                }
            }
            candidates
        };

        let tree_value =
            serde_json::to_value(&working).map_err(|err| KioError::schema(err.to_string()))?;
        let (tree_hash, _) = self.store.write_json(ObjectKind::Tree, &tree_value)?;
        let stats = commit_stats(prior_tree.as_ref(), &working);

        // A resurrection candidate forces a real commit even when the tree is
        // byte-identical to HEAD's (LC22-26): otherwise the raw bytes we just
        // republished into CAS above would have no distinguishing
        // `resurrection_commit` to retire the marker against, and the
        // tombstone/receipt would stay active indefinitely despite the raw
        // object being alive again.
        let (current_tool_lock_hash, current_tool_lock_bytes) = self.tool_lock_identity()?;
        if !force_commit
            && resurrection_candidates.is_empty()
            && head_tree_hash.as_deref() == Some(tree_hash.as_str())
            && head_commit
                .as_ref()
                .is_some_and(|commit| commit.tool_lock_hash == current_tool_lock_hash)
        {
            // Eligible no-ops have completed all immutable preparation at
            // this point. Publish their scheduler checkpoint before returning
            // so a failed state CAS can never follow an already-advanced ref.
            if scheduled_bound {
                wait_at_bound_snapshot_auto_barrier(self.pre_checkpoint_ready());
                self.store.validate_bound_layout()?;
                if let Some(authority) = expected_scheduled_authority {
                    self.recheck_scheduled_snapshot_authority(authority)?;
                }
                if let Some(policy) = expected_snapshot_policy {
                    policy.recheck(
                        self.bound_root.as_deref().expect("scheduled bound root"),
                        self.bound_kio.as_deref().expect("scheduled bound kio"),
                    )?;
                }
                self.reject_scheduled_bound_purge_state()?;
                self.reject_scheduled_marker_targets(
                    working.entries.iter().map(|entry| &entry.raw_hash),
                )?;
            }
            if let Some(publish) = before_ref_publication {
                let checkpoint = publish()?;
                if scheduled_bound {
                    self.store.validate_bound_layout()?;
                    if let Some(policy) = expected_snapshot_policy {
                        policy.recheck(
                            self.bound_root.as_deref().expect("scheduled bound root"),
                            self.bound_kio.as_deref().expect("scheduled bound kio"),
                        )?;
                    }
                    checkpoint.recheck(
                        self.bound_kio.as_deref().expect("scheduled bound kio"),
                        self.control(),
                    )?;
                    wait_at_bound_snapshot_auto_barrier(self.after_state_write_ready());
                    self.store.validate_bound_layout()?;
                    if let Some(authority) = expected_scheduled_authority {
                        self.recheck_scheduled_snapshot_authority(authority)?;
                    }
                    if let Some(policy) = expected_snapshot_policy {
                        policy.recheck(
                            self.bound_root.as_deref().expect("scheduled bound root"),
                            self.bound_kio.as_deref().expect("scheduled bound kio"),
                        )?;
                    }
                    checkpoint.recheck(
                        self.bound_kio.as_deref().expect("scheduled bound kio"),
                        self.control(),
                    )?;
                    self.reject_scheduled_bound_purge_state()?;
                    self.reject_scheduled_marker_targets(
                        working.entries.iter().map(|entry| &entry.raw_hash),
                    )?;
                }
            }
            if let Some(guard) = publication_guard {
                guard()?;
            }
            return Ok(SnapshotOutcome {
                noop: true,
                message: "snapshot noop: tree unchanged".to_owned(),
                tree_hash,
                commit_hash: None,
                commit: None,
                stats,
            });
        }

        let published_tool_lock_hash = self
            .store
            .write_content_object(ContentObjectKind::Toollock, &current_tool_lock_bytes)?;
        if published_tool_lock_hash != current_tool_lock_hash {
            return Err(KioError::schema(
                "published immutable tool-lock hash differs from snapshot authority",
            ));
        }

        let created_at = fixed_now
            .map(str::to_owned)
            .unwrap_or_else(|| self.now_utc_seconds());
        let message = message
            .map(str::to_owned)
            .unwrap_or_else(|| match commit_type {
                CommitType::Auto => format!("index auto snapshot at {created_at}"),
                _ => format!("snapshot at {created_at}"),
            });
        let parent = head_hash.clone();
        let commit = if commit_type == CommitType::Purged {
            CommitObject::new_purged(
                tree_hash.clone(),
                parent.clone(),
                created_at,
                message,
                current_tool_lock_hash,
                stats.clone(),
                purged_raws.to_vec(),
            )?
        } else {
            CommitObject::new(
                tree_hash.clone(),
                parent.clone(),
                created_at,
                message,
                current_tool_lock_hash,
                stats.clone(),
                commit_type,
            )?
        };
        let commit_value =
            serde_json::to_value(&commit).map_err(|err| KioError::schema(err.to_string()))?;
        // Content-addressed and therefore harmless/idempotent even when
        // `publish_ref` is false: the object becomes durable in the CAS but is
        // not yet reachable from any ref (05-runtime.md §3.5's `planned_commit`
        // — LC48 — computed and fixed once at the purge journal's `prepared`
        // phase, published for real later at `committed`).
        let (commit_hash, _) = self.store.write_json(ObjectKind::Commit, &commit_value)?;

        if !publish_ref {
            return Ok(SnapshotOutcome {
                noop: false,
                message: "snapshot computed (ref not published)".to_owned(),
                tree_hash,
                commit_hash: Some(commit_hash),
                commit: Some(commit),
                stats,
            });
        }

        // A scheduled checkpoint is a prepared eligible-attempt record.  The
        // commit object is already immutable and durable, but no ref/manifest
        // has moved yet.  If its conditional state publication fails, return
        // without making the object reachable.
        if scheduled_bound {
            wait_at_bound_snapshot_auto_barrier(self.pre_checkpoint_ready());
            self.store.validate_bound_layout()?;
            if let Some(authority) = expected_scheduled_authority {
                self.recheck_scheduled_snapshot_authority(authority)?;
            }
            if let Some(policy) = expected_snapshot_policy {
                policy.recheck(
                    self.bound_root.as_deref().expect("scheduled bound root"),
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                )?;
            }
            self.reject_scheduled_bound_purge_state()?;
            self.reject_scheduled_marker_targets(
                working.entries.iter().map(|entry| &entry.raw_hash),
            )?;
        }
        let checkpoint = match before_ref_publication {
            Some(publish) => Some(publish()?),
            None => None,
        };

        let manifest = self.manifest_value(&working, prior_tree.as_ref())?;
        let published_authority = if scheduled_bound {
            self.store.validate_bound_layout()?;
            if let Some(authority) = expected_scheduled_authority {
                self.recheck_scheduled_snapshot_authority(authority)?;
            }
            if let Some(policy) = expected_snapshot_policy {
                policy.recheck(
                    self.bound_root.as_deref().expect("scheduled bound root"),
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                )?;
            }
            if let Some(checkpoint) = checkpoint.as_ref() {
                checkpoint.recheck(
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                    self.control(),
                )?;
            }
            wait_at_bound_snapshot_auto_barrier(self.after_state_write_ready());
            self.store.validate_bound_layout()?;
            if let Some(authority) = expected_scheduled_authority {
                self.recheck_scheduled_snapshot_authority(authority)?;
            }
            if let Some(policy) = expected_snapshot_policy {
                policy.recheck(
                    self.bound_root.as_deref().expect("scheduled bound root"),
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                )?;
            }
            if let Some(checkpoint) = checkpoint.as_ref() {
                checkpoint.recheck(
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                    self.control(),
                )?;
            }
            self.reject_scheduled_bound_purge_state()?;
            self.reject_scheduled_marker_targets(
                working.entries.iter().map(|entry| &entry.raw_hash),
            )?;
            self.publish_with_journal(parent.as_deref(), &commit_hash, &commit, manifest.clone())?;
            Some(self.capture_scheduled_snapshot_authority()?)
        } else {
            if let Some(guard) = publication_guard {
                guard()?;
            }
            self.publish_with_journal(parent.as_deref(), &commit_hash, &commit, manifest.clone())?;
            None
        };
        if scheduled_bound {
            self.store.validate_bound_layout()?;
            if let Some(authority) = expected_scheduled_authority {
                self.recheck_scheduled_published_authority(
                    authority,
                    published_authority
                        .as_ref()
                        .expect("scheduled published authority binding"),
                    &commit_hash,
                )?;
            }
            if let Some(policy) = expected_snapshot_policy {
                policy.recheck(
                    self.bound_root.as_deref().expect("scheduled bound root"),
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                )?;
            }
            if let Some(checkpoint) = checkpoint.as_ref() {
                checkpoint.recheck(
                    self.bound_kio.as_deref().expect("scheduled bound kio"),
                    self.control(),
                )?;
            }
        }

        // U19/LC23-LC25: retire is appended only *after* this snapshot's
        // finalize (commit + ref publish + manifest, all now durable above) —
        // "same locked mutation" is satisfied because `_lock` (reentrant) is
        // still held. A crash before this point leaves the tombstone/receipt
        // active (LC24, safe side); the next locked mutation touching this
        // raw_hash (this same code path, naturally re-run) or an explicit
        // `kio repair verify-objects` backfill (LC27) completes it later.
        if !resurrection_candidates.is_empty() {
            let purge = self.purge_state()?;
            let actor = std::env::var("USER").unwrap_or_else(|_| "local-user".to_owned());
            purge.retire_resurrected(
                &resurrection_candidates,
                &commit_hash,
                &commit.created_at,
                &actor,
            )?;
        }

        Ok(SnapshotOutcome {
            noop: false,
            message: "snapshot created".to_owned(),
            tree_hash,
            commit_hash: Some(commit_hash),
            commit: Some(commit),
            stats,
        })
    }
}
