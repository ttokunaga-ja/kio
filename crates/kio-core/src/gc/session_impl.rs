use super::*;

impl GcSweepSession {
    fn snapshot_exchange_authority(
        &self,
        snapshot: &SnapshotAutoBinding,
        gc: &GcAutomationBinding,
    ) -> Result<SnapshotExchangeAuthority> {
        self.recheck_binding()?;
        snapshot.recheck(&self.scope, &self.kio)?;
        if !snapshot.config.is_some_and(|config| config.enabled)
            || self.automation_binding()? != *gc
        {
            return Err(snapshot_auto_authority_changed());
        }
        let (head, head_observation) = read_regular_observed(&self.kio, "HEAD", MAX_METADATA)?;
        let head_text = std::str::from_utf8(&head)
            .map_err(|_| corrupt("snapshot exchange HEAD is not UTF-8"))?;
        if head_text != "unborn\n" && !head_text.strip_suffix('\n').is_some_and(is_hash) {
            return Err(corrupt("snapshot exchange HEAD is invalid"));
        }
        let (tools, tool_observation) =
            read_regular_observed(&self.kio, "tool-lock.json", MAX_METADATA)?;
        let tool_value: serde_json::Value =
            serde_json::from_slice(&tools).map_err(|e| KioError::schema(e.to_string()))?;
        crate::scope::canonical_tool_lock_value(&tool_value)?;
        let binding = hash_bytes(&canonical_json_bytes(&json!({
            "protocol":"windows-snapshot-state-exchange-v1",
            "scope": id_file(&self.scope)?, "kio": id_file(&self.kio)?,
            "head": head_observation.digest, "tool_lock": tool_observation.digest,
            "config": snapshot.config_observation.digest, "ignore": snapshot.root_ignore_observation.as_ref().map(|observation| &observation.digest),
            "gc_policy": gc.gc_config_digest,
        }))?);
        if read_regular_observed(&self.kio, "HEAD", MAX_METADATA)?.1 != head_observation
            || read_regular_observed(&self.kio, "tool-lock.json", MAX_METADATA)?.1
                != tool_observation
        {
            return Err(snapshot_auto_authority_changed());
        }
        snapshot.recheck(&self.scope, &self.kio)?;
        self.recheck_binding()?;
        Ok(SnapshotExchangeAuthority {
            binding: binding[7..].to_owned(),
            head_observation,
            tool_observation,
        })
    }
    fn validate_pending_marker_data(
        &self,
        source: Option<GcInProgressMarker>,
        target: GcInProgressMarker,
    ) -> Result<()> {
        if let Some(source) = source {
            validate_marker_transition(&source, &target)?;
        }
        // In the terminal placement no predecessor remains. Only validate the
        // retained target and current truth; do not reconstruct old authority.
        let receipts = read_shallow_receipts_bound(&self.kio)?;
        let frozen = target
            .candidates
            .iter()
            .map(|candidate| (&candidate.commit_hash, &candidate.tree_hash))
            .collect();
        self.validate_marker_phase_state(&target, &receipts, &frozen)?;
        if let Some(expected) = &target.operation_receipts_digest
            && operation_receipt_observation_digest_bound(&self.kio, &target)? != *expected
        {
            return Err(corrupt("pending marker receipt identity/content changed"));
        }
        self.validate_frozen_marker_truth_without_public_read(&target)
    }

    /// Recheck the managed-restore exclusion using the retained `.kio`
    /// capability.  Public mutation entry points call this after their caller
    /// holds the bound writer lock, closing the bind-to-first-write race.
    fn ensure_no_pending_managed_restore_under_lock(&self) -> Result<()> {
        let directory = StoreDirectory::from_retained_with_control(
            self.kio
                .try_clone()
                .map_err(|error| ioerr(error, &self.root))?,
            self.root.join(".kio"),
            self.control.clone(),
        )?;
        reject_pending_managed_restore(&directory)
    }

    fn inject_gc_tree_fault(&self, _point: GcTreeFaultPoint) -> Result<()> {
        #[cfg(debug_assertions)]
        if self
            .control
            .debug()
            .core
            .gc_fault
            .known()
            .is_some_and(|fault| _point.matches(*fault))
        {
            return Err(KioError::new(
                "KIO-E-GC-TEST-INTERRUPTED-001",
                "GC test fault injection interrupted the sweep",
                json!({"point": _point.label()}),
                ExitCode::Interrupted,
            ));
        }
        Ok(())
    }

    fn wait_at_gc_tree_quarantine_barrier(&self) {
        #[cfg(debug_assertions)]
        wait_at_gc_test_barrier(
            self.control
                .debug()
                .core
                .gc_tree_quarantine_ready
                .as_deref(),
        );
    }

    fn wait_at_gc_before_state_write_barrier(&self) {
        #[cfg(debug_assertions)]
        wait_at_gc_test_barrier(
            self.control
                .debug()
                .core
                .snapshot_before_state_write_ready
                .as_deref(),
        );
    }

    fn wait_at_gc_pre_quarantine_barrier(&self) {
        #[cfg(debug_assertions)]
        wait_at_gc_test_barrier(self.control.debug().core.gc_pre_quarantine_ready.as_deref());
    }

    pub fn read_marker(&self) -> Result<Option<GcInProgressMarker>> {
        read_active_marker_bound(&self.kio, &self.control)
    }
    pub fn has_pending_marker_exchange(&self) -> Result<bool> {
        self.recheck_binding()?;
        has_pending_marker_exchange(&self.kio, &self.control)
    }

    /// Read-only query for coordinators which must enter locked recovery even
    /// when a pending exchange temporarily removed the public marker.
    pub fn has_active_or_pending_marker(&self) -> Result<bool> {
        self.recheck_binding()?;
        if has_pending_marker_exchange(&self.kio, &self.control)? {
            return Ok(true);
        }
        Ok(self.read_marker()?.is_some())
    }

    /// Recover a marker exchange while the caller holds this session's writer
    /// lock. Read-only callers must use `has_active_or_pending_marker` instead.
    pub fn recover_pending_marker_under_lock(&self) -> Result<()> {
        self.recheck_binding()?;
        reject_pending_snapshot_state_exchange(&self.kio, &self.control)?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        crate::platform::recover_marker(&self.kio, &self.control, |source, target| {
            self.validate_pending_marker_data(source, target)
        })
    }

    pub fn acquire_store_lock(&self) -> Result<BoundStoreLock> {
        self.recheck_binding()?;
        let lock = crate::scope::acquire_bound_store_lock_with_control(&self.kio, &self.control)?;
        reject_pending_snapshot_state_exchange(&self.kio, &self.control)?;
        Ok(lock)
    }
    /// Read-only pending-state query; never treats a missing public state as fresh.
    pub fn has_pending_snapshot_auto_state_exchange(&self) -> Result<bool> {
        self.recheck_binding()?;
        has_pending_snapshot_state_exchange(&self.kio, &self.control)
    }

    /// Acquire the scheduler recovery barrier, allowing only its own pending journal.
    pub fn acquire_snapshot_auto_recovery_lock(&self) -> Result<BoundReentrantStoreLock> {
        self.recheck_binding()?;
        let lock = crate::scope::acquire_bound_reentrant_store_lock_with_control(
            &self.lock_session,
            &self.kio,
            &self.control,
        )?;
        self.recheck_binding()?;
        if has_pending_marker_exchange(&self.kio, &self.control)? {
            return Err(corrupt("pending GC marker blocks scheduler recovery"));
        }
        if let Some(marker) = read_marker_without_pending_check(&self.kio)? {
            return Err(active_sweep_error(&marker));
        }
        self.ensure_no_pending_managed_restore_under_lock()?;
        Ok(lock)
    }

    /// Explicit recovery under the scheduler recovery writer lock, before state admission.
    pub fn recover_snapshot_auto_state_under_lock(
        &self,
        snapshot: &SnapshotAutoBinding,
        gc: &GcAutomationBinding,
    ) -> Result<()> {
        self.recheck_binding()?;
        if has_pending_marker_exchange(&self.kio, &self.control)? {
            return Err(corrupt("pending GC marker blocks scheduler recovery"));
        }
        if let Some(marker) = read_marker_without_pending_check(&self.kio)? {
            return Err(active_sweep_error(&marker));
        }
        self.ensure_no_pending_managed_restore_under_lock()?;
        snapshot.recheck(&self.scope, &self.kio)?;
        if self.automation_binding()? != *gc {
            return Err(snapshot_auto_authority_changed());
        }
        crate::platform::recover_snapshot_state(
            &self.kio,
            &self.control,
            || self.snapshot_exchange_authority(snapshot, gc),
            || self.snapshot_auto_state().map(|_| ()),
        )?;
        self.recheck_binding()
    }

    /// Acquire the scheduler writer barrier relative to the retained `.kio`
    /// descriptor, then make nested [`Repository`] store-lock acquisitions
    /// reentrant through the explicitly shared operation session. No ambient fallback:
    /// platforms that cannot prove the descriptor-relative lock boundary fail
    /// closed through the underlying bound-lock primitive.
    pub fn acquire_snapshot_auto_store_lock(&self) -> Result<BoundReentrantStoreLock> {
        self.recheck_binding()?;
        let lock = crate::scope::acquire_bound_reentrant_store_lock_with_control(
            &self.lock_session,
            &self.kio,
            &self.control,
        )?;
        if let Err(error) = self.recheck_binding() {
            drop(lock);
            return Err(error);
        }
        if let Err(error) = ensure_no_active_sweep_bound(&self.kio, &self.control) {
            drop(lock);
            return Err(error);
        }
        Ok(lock)
    }
    /// Read the validated complete GC authority subtree and retain a canonical
    /// semantic digest for the publication-to-GC handoff. Both reads are
    /// descriptor-bound and bracketed by scope identity checks.
    pub fn automation_binding(&self) -> Result<GcAutomationBinding> {
        self.recheck_binding()?;
        let (bytes, _) = read_regular_observed(&self.kio, "config.toml", MAX_METADATA)?;
        let config = read_automation_config_bytes(&bytes)?;
        self.recheck_binding()?;
        let parsed = parse_config_bytes(&bytes)?;
        let gc = parsed
            .as_ref()
            .and_then(|value| value.get("gc"))
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| KioError::schema(error.to_string()))?
            .unwrap_or(serde_json::Value::Null);
        Ok(GcAutomationBinding {
            config,
            gc_config_digest: hash_bytes(&canonical_json_bytes(&gc)?),
            scope_identity: id_file(&self.scope)?,
            kio_identity: id_file(&self.kio)?,
        })
    }

    /// Read the complete validated configuration through the retained `.kio`
    /// capability and bind the exact file observation used for scheduled
    /// snapshot authorization.
    pub fn snapshot_auto_binding(&self) -> Result<SnapshotAutoBinding> {
        self.recheck_binding()?;
        let (bytes, config_observation) =
            read_regular_observed(&self.kio, "config.toml", MAX_METADATA)?;
        let config = read_snapshot_auto_config_bytes(&bytes)?;
        let root_ignore_observation =
            match read_regular_observed(&self.scope, ".kioignore", MAX_METADATA) {
                Ok((_, observation)) => Some(observation),
                Err(error) if is_io_not_found(&error) => None,
                Err(error) => return Err(error),
            };
        self.recheck_binding()?;
        Ok(SnapshotAutoBinding {
            config,
            config_observation,
            root_ignore_observation,
            scope_identity: id_file(&self.scope)?,
            kio_identity: id_file(&self.kio)?,
        })
    }

    /// Read the optional strict scheduler checkpoint relative to the retained
    /// store handle.  A missing file is the only representation of a first
    /// scheduled attempt.
    pub fn snapshot_auto_state(&self) -> Result<SnapshotAutoStateBinding> {
        self.recheck_binding()?;
        reject_pending_snapshot_state_exchange(&self.kio, &self.control)?;
        let (state, observation) =
            match read_regular_observed(&self.kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA) {
                Ok((bytes, observation)) => {
                    (Some(parse_snapshot_auto_state(&bytes)?), Some(observation))
                }
                Err(error) if is_io_not_found(&error) => (None, None),
                Err(error) => return Err(error),
            };
        self.recheck_binding()?;
        Ok(SnapshotAutoStateBinding { state, observation })
    }

    /// Atomically and durably publish the scheduler checkpoint at the prepared
    /// eligible-attempt boundary. `expected` must be the exact state observation
    /// re-read under the writer lock. A replacement between eligibility and
    /// publication fails closed before a scheduled ref can advance.
    pub fn publish_snapshot_auto_state(
        &self,
        expected: &SnapshotAutoStateBinding,
        now: &str,
        working_set_digest: &str,
        record_eligible_attempt: bool,
    ) -> Result<SnapshotAutoStateBinding> {
        self.prepare_snapshot_auto_state_publication(expected)?;
        let seconds = parse_utc_seconds(now)
            .filter(|seconds| format_utc_seconds(*seconds) == now)
            .ok_or_else(|| {
                snapshot_auto_clock_error("current time is not canonical UTC seconds")
            })?;
        if !is_hash(working_set_digest) {
            return Err(corrupt(
                "snapshot auto working-set digest is not canonical sha256",
            ));
        }
        if expected.state.is_none() && !record_eligible_attempt {
            return Err(corrupt(
                "first snapshot auto state publication must record an eligible attempt",
            ));
        }
        if let Some(previous) = expected.state.as_ref() {
            let previous_seconds = parse_utc_seconds(&previous.last_successful_eligible_attempt_at)
                .ok_or_else(|| corrupt("snapshot auto state time is invalid"))?;
            let observed_since_seconds = parse_utc_seconds(&previous.idle_observed_since)
                .ok_or_else(|| corrupt("snapshot auto state idle time is invalid"))?;
            if seconds < previous_seconds || seconds < observed_since_seconds {
                return Err(snapshot_auto_clock_error(
                    "scheduled snapshot clock moved backwards",
                ));
            }
        }
        let previous = expected.state.as_ref();
        let state = SnapshotAutoState {
            version: SNAPSHOT_AUTO_STATE_VERSION,
            last_successful_eligible_attempt_at: match previous {
                Some(state) if !record_eligible_attempt => {
                    state.last_successful_eligible_attempt_at.clone()
                }
                _ => format_utc_seconds(seconds),
            },
            working_set_digest: working_set_digest.to_owned(),
            idle_observed_since: match previous {
                Some(state) if state.working_set_digest == working_set_digest => {
                    state.idle_observed_since.clone()
                }
                _ => format_utc_seconds(seconds),
            },
        };
        let bytes = canonical_snapshot_auto_state_bytes(&state)?;
        let temporary = unique_internal_name(".snapshot-auto-state");
        create_new_bound(&self.kio, &temporary, &bytes, MAX_METADATA)?;
        let (_, temporary_observation) =
            read_regular_observed(&self.kio, &temporary, MAX_METADATA)?;
        if let Err(error) = self.recheck_binding() {
            retire_snapshot_state_temporary(
                &self.kio,
                &temporary,
                &bytes,
                &temporary_observation,
                &self.control,
            )?;
            return Err(error);
        }
        let observed_state = match self.snapshot_auto_state() {
            Ok(state) => state,
            Err(error) => {
                retire_snapshot_state_temporary(
                    &self.kio,
                    &temporary,
                    &bytes,
                    &temporary_observation,
                    &self.control,
                )?;
                return Err(error);
            }
        };
        if &observed_state != expected {
            retire_snapshot_state_temporary(
                &self.kio,
                &temporary,
                &bytes,
                &temporary_observation,
                &self.control,
            )?;
            return Err(snapshot_auto_state_changed());
        }
        self.wait_at_gc_before_state_write_barrier();
        match expected.observation.as_ref() {
            None => {
                if let Err(error) = rename_snapshot_state_noreplace(
                    &self.kio,
                    &temporary,
                    SNAPSHOT_AUTO_STATE_LEAF,
                    &self.control,
                ) {
                    let state_changed = self
                        .snapshot_auto_state()
                        .map(|state| &state != expected)
                        .unwrap_or(true);
                    retire_snapshot_state_temporary(
                        &self.kio,
                        &temporary,
                        &bytes,
                        &temporary_observation,
                        &self.control,
                    )?;
                    if state_changed {
                        return Err(snapshot_auto_state_changed());
                    }
                    return Err(error);
                }
            }
            Some(observation) => self.replace_snapshot_state(
                &temporary,
                &bytes,
                observation,
                &temporary_observation,
            )?,
        }
        sync_bound_directory(&self.kio, SNAPSHOT_AUTO_STATE_LEAF)?;
        let published = self.snapshot_auto_state()?;
        if published.state.as_ref() != Some(&state) {
            return Err(corrupt("snapshot auto state changed during publication"));
        }
        Ok(published)
    }

    pub(super) fn replace_snapshot_state(
        &self,
        temporary: &str,
        bytes: &[u8],
        expected: &FileObservation,
        temporary_observation: &FileObservation,
    ) -> Result<()> {
        crate::platform::replace_snapshot_state(
            &self.kio,
            &self.control,
            temporary,
            bytes,
            expected,
            temporary_observation,
            || {
                let snapshot = self.snapshot_auto_binding()?;
                let gc_binding = self.automation_binding()?;
                let binding = self.snapshot_exchange_authority(&snapshot, &gc_binding)?;
                Ok((binding, move || {
                    self.snapshot_exchange_authority(&snapshot, &gc_binding)
                }))
            },
        )
    }

    /// Establish the scheduler-state publication precondition while the
    /// caller owns the scheduler writer barrier.  In particular, malformed or
    /// substituted private checkpoint crash residue is rejected *before* a
    /// scheduled snapshot can publish any CAS object or ref.  A process that
    /// died after fsyncing a valid private checkpoint is recovered by removing
    /// that exact, descriptor-relative record.
    ///
    /// This deliberately does not publish the new checkpoint; callers use it
    /// before immutable snapshot preparation and then call
    /// [`Self::publish_snapshot_auto_state`] at the pre-ref prepared-attempt
    /// boundary.
    pub fn prepare_snapshot_auto_state_publication(
        &self,
        expected: &SnapshotAutoStateBinding,
    ) -> Result<()> {
        self.recheck_binding()?;
        if &self.snapshot_auto_state()? != expected {
            return Err(snapshot_auto_state_changed());
        }
        cleanup_snapshot_auto_state_temporaries(&self.kio, &self.control)?;
        // Cleanup has no reason to modify the public state leaf.  Re-observe
        // it anyway so a competing namespace writer cannot race the cleanup
        // precondition and leave a commit published before its checkpoint
        // conflict is discovered.
        if &self.snapshot_auto_state()? != expected {
            return Err(snapshot_auto_state_changed());
        }
        self.recheck_binding()
    }

    /// Retained scope-root capability for the scheduler's direct-file scan.
    pub fn retained_scope_handle(&self) -> Result<std::fs::File> {
        self.recheck_binding()?;
        self.scope
            .try_clone()
            .map_err(|error| ioerr(error, "scope"))
    }

    /// Reject unsafe direct entries before a scheduled scan. Direct child
    /// directories remain outside this scope's working tree; symlinks,
    /// reparse points, hardlinks and other special leaves are never silently
    /// interpreted as document inputs.
    pub fn validate_snapshot_auto_direct_entries(&self) -> Result<BTreeSet<String>> {
        self.recheck_binding()?;
        let names = validated_snapshot_auto_direct_entries(&self.scope)?;
        self.recheck_binding()?;
        Ok(names)
    }
    pub fn ensure_index_rotation_supported(&self) -> Result<()> {
        self.recheck_binding()?;
        Ok(())
    }
    /// Mint the process-local capability required by
    /// [`Self::remove_candidate_tree`].
    ///
    /// # Safety
    ///
    /// Immediately before this call, the caller must descriptor-bind the
    /// public SQLite leaf and verify its strict durable GC attestation against
    /// this marker's sweep ID, plan digest, initial generation, pre-sweep
    /// generation, physical identity, and rotation role.  `kio-index` is the
    /// trusted coordinator that performs that verification; marker fields
    /// alone are not sufficient.
    pub unsafe fn authorize_tree_removal_after_index_attestation(
        &self,
        marker: &GcInProgressMarker,
        attested_index_file: Option<&std::fs::File>,
    ) -> Result<GcTreeRemovalPermit> {
        self.recheck_binding()?;
        if self.read_marker()?.as_ref() != Some(marker) {
            return Err(corrupt("GC marker changed before index authorization"));
        }
        let (index_file, index_state) = match (marker.index_pre_sweep.as_ref(), attested_index_file)
        {
            (Some(GcIndexState::Absent), None) => (None, None),
            (Some(GcIndexState::Present { identity, .. }), Some(file)) => {
                let metadata = cap_fs::Metadata::from_file(file)
                    .map_err(|error| ioerr(error, "index/sqlite.db"))?;
                valid_file(&metadata, u64::MAX)?;
                if canonical_gc_index_identity_from_metadata(&metadata)? != *identity {
                    return Err(corrupt("attested GC index identity differs from marker"));
                }
                (
                    Some(
                        file.try_clone()
                            .map_err(|error| ioerr(error, "index/sqlite.db"))?,
                    ),
                    Some(file_state(&metadata)),
                )
            }
            _ => return Err(corrupt("GC index attestation handle/state mismatch")),
        };
        Ok(GcTreeRemovalPermit {
            kio_identity: id_file(&self.kio)?,
            marker_digest: hash_bytes(&marker.canonical_bytes()?),
            index_file,
            index_state,
        })
    }
    /// Re-plan through the same retained scope/.kio descriptors used for a
    /// subsequent mutation, closing the public-path rebind gap after locking.
    pub fn plan_at(&self, now: i64) -> Result<GcPlan> {
        self.recheck_binding()?;
        let plan = self.bound_planner()?.plan_at(now)?;
        self.recheck_binding()?;
        Ok(plan)
    }
    /// Validate the marker phase against exact durable receipts and tree
    /// presence. Impossible combinations are corruption, not resumable state.
    pub fn validate_recovery_state(&self, marker: &GcInProgressMarker) -> Result<GcSweepProgress> {
        if self.read_marker()?.as_ref() != Some(marker) {
            return Err(corrupt("GC marker changed during recovery validation"));
        }
        let receipts = read_shallow_receipts_bound(&self.kio)?;
        let frozen: BTreeMap<_, _> = marker
            .candidates
            .iter()
            .map(|candidate| (&candidate.commit_hash, &candidate.tree_hash))
            .collect();
        let progress = self.validate_marker_phase_state(marker, &receipts, &frozen)?;
        if let Some(expected) = &marker.operation_receipts_digest {
            let actual = operation_receipt_observation_digest_bound(&self.kio, marker)?;
            if &actual != expected {
                return Err(corrupt("marker-owned receipt identity/content changed"));
            }
        }
        Ok(progress)
    }
    /// Structural phase validation for fsck/read paths. It intentionally does
    /// not compare live baseline identity digests, which are an executor-only
    /// authorization check performed by `validate_frozen_marker_current_truth`.
    pub fn validate_marker_phase_state(
        &self,
        marker: &GcInProgressMarker,
        receipts: &BTreeMap<String, ShallowReceipt>,
        frozen: &BTreeMap<&String, &String>,
    ) -> Result<GcSweepProgress> {
        let mut receipt_count = 0;
        for (commit, receipt) in receipts {
            if let Some(tree) = frozen.get(commit) {
                let expected = ShallowReceipt::new(
                    (*commit).clone(),
                    (**tree).clone(),
                    marker.started_at.clone(),
                )?;
                if receipt != &expected {
                    return Err(corrupt("recovery receipt tree differs from frozen marker"));
                }
                receipt_count += 1;
            }
        }
        let (any_tree_missing, all_trees_missing) = self.marker_tree_presence(marker)?;
        let progress = GcSweepProgress {
            receipt_count,
            receipts_complete: receipt_count == marker.candidates.len(),
            any_tree_missing,
            all_trees_missing,
        };
        // Tree disappearance is irreversible.  It is never a valid durable
        // state until the pre-sweep index generation has been committed.
        if any_tree_missing && marker.index_pre_sweep.is_none() {
            return Err(corrupt(
                "GC tree is missing before durable pre-sweep index rotation",
            ));
        }
        if any_tree_missing {
            self.require_live_pre_sweep_index_binding(marker)?;
        }
        let all_trees_present = !any_tree_missing;
        let index_final_is_none = marker.index_final.is_none();
        let valid = match marker.phase {
            GcSweepPhase::Prepared => {
                receipt_count == 0 && all_trees_present && index_final_is_none
            }
            GcSweepPhase::Receipting => all_trees_present && index_final_is_none,
            GcSweepPhase::Sweeping => progress.receipts_complete && index_final_is_none,
            // A crash immediately after persisting `finalizing` but before the
            // caller rotates the index is resumable. `Some` records a completed
            // final rotation; both forms require the physical sweep complete.
            GcSweepPhase::Finalizing => progress.receipts_complete && all_trees_missing,
        };
        if !valid {
            return Err(corrupt(
                "GC marker phase contradicts durable receipt/tree/index state",
            ));
        }
        Ok(progress)
    }
    /// Alias kept intentionally terse for orchestrators deciding whether an
    /// intact marker can be discarded after a fresh locked preview.
    pub fn progress(&self, marker: &GcInProgressMarker) -> Result<GcSweepProgress> {
        self.validate_recovery_state(marker)
    }
    /// True only before any irreversible receipt/tree work. A caller may remove
    /// this marker after a fresh locked plan differs; all other states retain
    /// their receipts and must either resume or fail closed.
    pub fn marker_can_be_discarded_after_fresh_replan(
        &self,
        marker: &GcInProgressMarker,
    ) -> Result<bool> {
        let progress = self.validate_recovery_state(marker)?;
        Ok(progress.receipt_count == 0 && !progress.any_tree_missing)
    }
    /// Re-read refs, commits and sharing from the currently bound store.  This
    /// never trusts the marker's old truth digest as permission to continue.
    /// It permits progress that the marker itself caused (new matching receipts)
    /// but rejects a new ref or non-frozen commit sharing a victim tree.
    pub fn validate_frozen_marker_current_truth(&self, marker: &GcInProgressMarker) -> Result<()> {
        let _ = self.validate_recovery_state(marker)?;
        self.validate_frozen_marker_truth_without_public_read(marker)
    }

    fn validate_frozen_marker_truth_without_public_read(
        &self,
        marker: &GcInProgressMarker,
    ) -> Result<()> {
        let planner = self.bound_planner()?;
        let mut stats = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let scope_bytes = read_regular_observed(&self.kio, "scope.json", MAX_METADATA)?;
        observations.insert("scope.json".into(), scope_bytes.1.clone());
        let config_bytes = read_regular_observed(&self.kio, "config.toml", MAX_METADATA)?;
        observations.insert("config.toml".into(), config_bytes.1.clone());
        let refs = planner.read_refs(&mut stats, &mut observations)?;
        let commits = planner.inventory_commits(&mut stats, &mut observations)?;
        validate_commit_links(&commits)?;
        let scope_digest = validate_scope_bytes(&scope_bytes.0)?;
        let (policy, config_digest) = read_policy_bytes(&config_bytes.0)?;
        if semantic_stable_truth_digest(
            &scope_digest,
            &config_digest,
            &policy,
            &refs,
            &commits,
            &observations,
        )? != marker.stable_truth_digest
        {
            return Err(corrupt("immutable GC recovery truth changed"));
        }
        let frozen: BTreeMap<_, _> = marker
            .candidates
            .iter()
            .map(|candidate| (&candidate.commit_hash, &candidate.tree_hash))
            .collect();
        if read_receipt_observation_digest_bound(&self.kio, &frozen)?
            != marker.baseline_receipts_digest
        {
            return Err(corrupt(
                "non-operation shallow receipts changed during recovery",
            ));
        }
        for (commit, tree) in &frozen {
            let current = commits
                .get(*commit)
                .ok_or_else(|| corrupt("frozen GC commit is missing"))?;
            if &current.tree != *tree {
                return Err(corrupt("frozen GC commit tree changed"));
            }
        }
        if refs.values().any(|tip| frozen.contains_key(tip)) {
            return Err(corrupt("current ref points to a frozen shallow candidate"));
        }
        if read_receipt_observation_digest_bound(&self.kio, &frozen)?
            != marker.baseline_receipts_digest
        {
            return Err(corrupt(
                "non-operation shallow receipts changed during recovery",
            ));
        }
        for tree in &marker.trees {
            for (hash, commit) in &commits {
                if &commit.tree == tree && !frozen.contains_key(hash) {
                    return Err(corrupt("non-frozen commit shares GC victim tree"));
                }
            }
        }
        // A marker is recovery state, never an authorization token.  In
        // particular, do not merely check that its old digests still look
        // plausible: a same-user writer could manufacture a canonical marker
        // and matching receipts for a protected tree.  Re-run the retention
        // selection from the current retained config/refs/commit graph and
        // require its *entire* frozen pair set to match.  Marker-owned
        // receipts are deliberately treated as virtual pre-sweep state, so a
        // legitimate crash after receipt publication (or tree removal) can
        // resume; all other receipts remain already-shallow input.
        let current_receipts = read_shallow_receipts_bound(&self.kio)?;
        let mut baseline_receipts = HashMap::new();
        for (commit, receipt) in current_receipts {
            if !frozen.contains_key(&commit) {
                baseline_receipts.insert(commit, receipt.tree_hash);
            }
        }
        validate_receipt_links(&baseline_receipts, &commits)?;
        let mut eligibility_stats = GcPlanStats::default();
        let mut exclusions = BTreeMap::new();
        let expected = retention_candidate_pairs(
            &policy,
            &refs,
            &commits,
            &baseline_receipts,
            parse_utc_seconds(&marker.started_at)
                .ok_or_else(|| corrupt("invalid marker timestamp"))?,
            &mut eligibility_stats,
            &planner.limits,
            &mut exclusions,
        )?;
        let actual: Vec<_> = marker
            .candidates
            .iter()
            .map(|candidate| (candidate.commit_hash.clone(), candidate.tree_hash.clone()))
            .collect();
        if actual != expected {
            return Err(corrupt(
                "GC marker candidates are not exactly authorized by current retention policy",
            ));
        }
        Ok(())
    }
    pub fn publish_marker(&self, marker: &GcInProgressMarker) -> Result<()> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let markers = ensure_child_dir(&internal, "markers")?;
        let staged = unique_internal_name(&format!("prepared-{}", marker.sweep_id));
        let bytes = marker.canonical_bytes()?;
        // Never expose a partially written public marker. The private stage is
        // create-new, fully written and fsynced before a no-clobber atomic
        // rename publishes `in_progress` in one namespace operation.
        publish_staged(
            &markers,
            &staged,
            &gc,
            "in_progress",
            &bytes,
            MAX_MARKER_BYTES,
            || self.inject_gc_tree_fault(GcTreeFaultPoint::MarkerStageFsync),
            &self.control,
        )?;

        sync_bound_directory(&gc, "in_progress")?;
        sync_bound_directory(&markers, &staged)?;
        let (published, _) = read_regular_observed(&gc, "in_progress", MAX_MARKER_BYTES)?;
        if published != bytes || GcInProgressMarker::parse_canonical(&published)? != *marker {
            return Err(corrupt("GC marker changed during atomic publication"));
        }
        Ok(())
    }
    pub fn advance_marker(&self, marker: &GcInProgressMarker) -> Result<()> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        let (current, observed) = self.read_marker_observed()?;
        validate_marker_transition(&current, marker)?;
        self.write_marker_expected(marker, &observed)
    }
    /// Bind every marker-owned receipt observation immediately before the
    /// caller advances Receipting -> Sweeping.
    pub fn bind_operation_receipts(
        &self,
        marker: &GcInProgressMarker,
    ) -> Result<GcInProgressMarker> {
        if marker.phase != GcSweepPhase::Receipting || marker.operation_receipts_digest.is_some() {
            return Err(corrupt(
                "operation receipt binding is only valid in receipting phase",
            ));
        }
        let mut bound = marker.clone();
        bound.operation_receipts_digest = Some(operation_receipt_observation_digest_bound(
            &self.kio, marker,
        )?);
        Ok(bound)
    }
    fn write_marker_expected(
        &self,
        marker: &GcInProgressMarker,
        expected: &FileObservation,
    ) -> Result<()> {
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let markers = ensure_child_dir(&internal, "markers")?;
        crate::platform::exchange_marker(
            &gc,
            &self.control,
            &internal,
            &markers,
            marker,
            expected,
            |source, target| self.validate_pending_marker_data(source, target),
        )
    }

    pub fn create_receipt(
        &self,
        candidate: &GcCandidate,
        at: String,
    ) -> Result<GcReceiptPublication> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        if !is_hash(&candidate.commit_hash) || !is_hash(&candidate.tree_hash) {
            return Err(corrupt("invalid GC candidate"));
        }
        let marker = self
            .read_marker()?
            .ok_or_else(|| corrupt("GC marker is missing"))?;
        if !marker.is_frozen_pair(&candidate.commit_hash, &candidate.tree_hash) {
            return Err(corrupt("candidate is not in frozen GC marker"));
        }
        // Marker-owned receipts are fully deterministic; callers cannot select
        // an alternate timestamp and later claim it belongs to this operation.
        let receipt = ShallowReceipt::new(
            candidate.commit_hash.clone(),
            candidate.tree_hash.clone(),
            marker.started_at.clone(),
        )?;
        if at != marker.started_at {
            return Err(corrupt("GC receipt timestamp differs from marker start"));
        }
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let shallowed = ensure_child_dir(&gc, "shallowed")?;
        let leaf = &candidate.commit_hash["sha256:".len()..];
        match read_regular_observed(&shallowed, leaf, MAX_METADATA) {
            Ok((existing, observed)) => {
                if ShallowReceipt::parse_canonical(&existing, leaf)? != receipt {
                    return Err(corrupt(
                        "existing shallow receipt differs from frozen candidate",
                    ));
                }
                // An exact pre-existing receipt is only an authorization once
                // its own retained handle and parent directory have been
                // forced durable.  Merely re-reading the pathname leaves a
                // crash window in which this invocation can start removing a
                // tree while the receipt has not reached stable storage.
                let durable = open_verified_receipt_durable_handle(&shallowed, leaf, &observed)?;
                durable.sync_all().map_err(|error| ioerr(error, leaf))?;
                verify_retained_receipt_observation(&durable, leaf, &observed)?;
                sync_bound_directory(&shallowed, leaf)?;
                Ok(GcReceiptPublication::AlreadyPresent)
            }
            Err(error) if is_io_not_found(&error) => {
                let internal = ensure_child_dir(&gc, "internal")?;
                let receipts = ensure_child_dir(&internal, "receipts")?;
                let staged = unique_internal_name(&format!("{}-{}", marker.sweep_id, leaf));
                let bytes = receipt.canonical_bytes()?;
                // As with the operation marker, stage and fsync the complete
                // record before its final receipt name exists. A crash during
                // staging leaves no malformed authorization at `shallowed/`.
                publish_staged(
                    &receipts,
                    &staged,
                    &shallowed,
                    leaf,
                    &bytes,
                    MAX_METADATA,
                    || self.inject_gc_tree_fault(GcTreeFaultPoint::ReceiptStageFsync),
                    &self.control,
                )?;

                sync_bound_directory(&shallowed, leaf)?;
                sync_bound_directory(&receipts, &staged)?;
                let (published, _) = read_regular_observed(&shallowed, leaf, MAX_METADATA)?;
                if published != bytes
                    || ShallowReceipt::parse_canonical(&published, leaf)? != receipt
                {
                    return Err(corrupt("GC receipt changed during atomic publication"));
                }
                Ok(GcReceiptPublication::NewlyPublished)
            }
            Err(error) => Err(error),
        }
    }
    /// Remove one tree after every candidate referencing it has a durable exact
    /// receipt. The leaf is read/hashed/schema-checked from its retained parent
    /// descriptor immediately before capability-relative unlink.
    pub fn remove_candidate_tree(
        &self,
        permit: &GcTreeRemovalPermit,
        marker: &GcInProgressMarker,
        tree_hash: &str,
    ) -> Result<bool> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        self.validate_tree_removal_permit(permit, marker)?;
        if marker.phase != GcSweepPhase::Sweeping
            || marker.index_pre_sweep.is_none()
            || marker.index_rotation.is_some()
        {
            return Err(corrupt(
                "GC tree removal requires durable pre-sweep index rotation",
            ));
        }
        self.require_live_pre_sweep_index_binding(marker)?;
        // Revalidate immutable truth and non-operation receipts for every
        // physical victim, not merely once at resume entry.
        self.validate_frozen_marker_current_truth(marker)?;
        let active = self
            .read_marker()?
            .ok_or_else(|| corrupt("GC marker is missing"))?;
        if active != *marker || !marker.trees.iter().any(|tree| tree == tree_hash) {
            return Err(corrupt("GC marker changed before tree removal"));
        }
        let receipts = read_shallow_receipts_bound(&self.kio)?;
        for candidate in marker
            .candidates
            .iter()
            .filter(|c| c.tree_hash == tree_hash)
        {
            let receipt = receipts
                .get(&candidate.commit_hash)
                .ok_or_else(|| corrupt("tree removal before every shared-tree receipt"))?;
            if receipt.tree_hash != tree_hash {
                return Err(corrupt("receipt tree does not match frozen candidate"));
            }
        }
        // Bind every frozen receipt used to authorize this victim twice through
        // retained descriptors. This rejects a replace/symlink/hardlink race
        // between recovery validation and the destructive move.
        self.recheck_tree_receipts(marker, tree_hash)?;
        let planner = self.bound_planner()?;
        let mut stats = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let refs = planner.read_refs(&mut stats, &mut observations)?;
        let commits = planner.inventory_commits(&mut stats, &mut observations)?;
        validate_commit_links(&commits)?;
        if refs.values().any(|tip| {
            commits
                .get(tip)
                .is_some_and(|commit| commit.tree == tree_hash)
        }) {
            return Err(corrupt(
                "current ref commit still references GC victim tree",
            ));
        }
        for (commit_hash, commit) in &commits {
            if commit.tree == tree_hash && !marker.is_frozen_pair(commit_hash, tree_hash) {
                return Err(corrupt("retained or new commit shares GC victim tree"));
            }
        }
        let raw = tree_hash
            .strip_prefix("sha256:")
            .filter(|x| x.len() == 64 && hex(x))
            .ok_or_else(|| corrupt("invalid tree hash"))?;
        let trees = open_path(&self.kio, "objects/trees")?;
        let a = open_required_dir(&trees, &raw[..2], "tree object fanout is missing")?;
        let d = open_required_dir(&a, &raw[2..4], "tree object fanout is missing")?;
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let archive_dir = ensure_child_dir(&internal, "trees")?;
        let quarantine = format!("{}-{}", marker.sweep_id, raw);
        let captured_name = format!("{}-{raw}-captured", marker.sweep_id);
        crate::platform::retire_candidate_tree(
            tree_hash,
            raw,
            &d,
            &archive_dir,
            &quarantine,
            &captured_name,
            |bytes| self.validate_committed_tree_bytes(marker, tree_hash, bytes),
            || self.require_active_marker(marker).map(|_| ()),
            || {
                self.wait_at_gc_pre_quarantine_barrier();
                self.validate_tree_removal_permit(permit, marker)?;
                self.require_live_pre_sweep_index_binding(marker)
            },
            || {
                self.inject_gc_tree_fault(GcTreeFaultPoint::TreeQuarantine)?;
                self.wait_at_gc_tree_quarantine_barrier();
                Ok(())
            },
            || {
                self.require_active_marker(marker)?;
                self.validate_tree_removal_permit(permit, marker)?;
                self.require_live_pre_sweep_index_binding(marker)?;
                self.recheck_tree_receipts(marker, tree_hash)
            },
            || self.inject_gc_tree_fault(GcTreeFaultPoint::TreeRetirementCapture),
            &self.control,
        )
    }

    pub fn remove_marker(&self, expected: &GcInProgressMarker) -> Result<()> {
        self.recheck_binding()?;
        self.ensure_no_pending_managed_restore_under_lock()?;
        let (current, observation) = self.read_marker_observed()?;
        if &current != expected {
            return Err(corrupt("GC marker changed before finalization"));
        }
        let gc = open_required_dir(&self.kio, "gc", "GC directory is missing")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let markers = ensure_child_dir(&internal, "markers")?;
        remove_completed_marker(&gc, &markers, expected, &observation, &self.control)
    }

    /// Check that the caller's public scope pathname still names exactly the
    /// retained capability handles. Invoke around any external index operation.
    pub fn assert_public_identity(&self) -> Result<()> {
        self.recheck_binding()
    }
    /// Borrow the retained `.kio` capability without reopening the public path.
    /// Index rotation and maintenance proof may derive bounded capabilities
    /// from it. Object deletion must use opaque planned-removal capabilities
    /// after current reachability proof, never direct raw-handle deletion.
    pub fn retained_kio_handle(&self) -> Result<std::fs::File> {
        self.recheck_binding()?;
        self.kio.try_clone().map_err(|error| ioerr(error, "kio"))
    }
    /// Validate final shallow receipts through this retained scope capability.
    pub fn validated_final_shallow_receipts(&self) -> Result<BTreeMap<String, String>> {
        self.reads.charge(0)?;
        self.recheck_binding()?;
        validated_final_shallow_receipts_bound(&self.bound_planner()?)
    }
    pub fn validate_final_shallow_tree(&self, commit_hash: &str, tree_hash: &str) -> Result<()> {
        validate_final_shallow_receipt(
            &self.validated_final_shallow_receipts()?,
            commit_hash,
            tree_hash,
        )
    }
    fn bound_planner(&self) -> Result<GcPlanner> {
        Ok(GcPlanner {
            reads: self.reads.clone(),
            control: self.control.clone(),
            root: self.root.clone(),
            scope: self.scope.try_clone().map_err(|e| ioerr(e, "scope"))?,
            kio: self.kio.try_clone().map_err(|e| ioerr(e, "kio"))?,
            limits: GcPlanLimits::default(),
        })
    }
    fn recheck_binding(&self) -> Result<()> {
        if id_file(&self.scope)? != id_path(&self.root)?
            || id_file(&self.kio)? != id_child(&self.scope, ".kio")?
        {
            Err(corrupt("scope changed during GC sweep"))
        } else {
            Ok(())
        }
    }
    fn read_marker_observed(&self) -> Result<(GcInProgressMarker, FileObservation)> {
        if has_pending_marker_exchange(&self.kio, &self.control)? {
            return Err(corrupt(
                "pending GC marker exchange requires locked recovery",
            ));
        }
        let gc = open_required_dir(&self.kio, "gc", "GC directory is missing")?;
        let (bytes, observed) = read_regular_observed(&gc, "in_progress", MAX_MARKER_BYTES)?;
        Ok((GcInProgressMarker::parse_canonical(&bytes)?, observed))
    }
    fn require_active_marker(&self, expected: &GcInProgressMarker) -> Result<FileObservation> {
        let (actual, observed) = self.read_marker_observed()?;
        if &actual != expected {
            return Err(corrupt(
                "GC marker changed immediately before tree mutation",
            ));
        }
        Ok(observed)
    }
    fn marker_tree_presence(&self, marker: &GcInProgressMarker) -> Result<(bool, bool)> {
        self.reads.charge(0)?;
        let trees = open_path(&self.kio, "objects/trees")?;
        let mut any_missing = false;
        let mut all_missing = true;
        for tree_hash in &marker.trees {
            let raw = tree_hash
                .strip_prefix("sha256:")
                .filter(|value| value.len() == 64 && hex(value))
                .ok_or_else(|| corrupt("invalid marker tree hash"))?;
            let canonical = match open_optional_dir(&trees, &raw[..2])? {
                None => false,
                Some(a) => match open_optional_dir(&a, &raw[2..4])? {
                    None => false,
                    Some(dir) => match read_cas_regular_observed(
                        &self.reads,
                        &dir,
                        raw,
                        MAX_TREE_OBJECT_BYTES,
                    ) {
                        Ok((bytes, _)) => {
                            self.validate_committed_tree_bytes(marker, tree_hash, &bytes)?;
                            true
                        }
                        Err(error) if is_io_not_found(&error) => false,
                        Err(error) => return Err(error),
                    },
                },
            };
            let archive = format!("{}-{raw}", marker.sweep_id);
            // Keep each directory capability alive through the complete read.  In
            // Rust 2021, the nested `if let` scrutinee temporaries lived until the
            // end of this initializer; make that lifetime explicit for Rust 2024.
            let (archived, captured) = {
                let gc = open_optional_dir(&self.kio, "gc")?;
                let internal = match gc.as_ref() {
                    Some(gc) => open_optional_dir(gc, "internal")?,
                    None => None,
                };
                let archives = match internal.as_ref() {
                    Some(internal) => open_optional_dir(internal, "trees")?,
                    None => None,
                };
                match archives.as_ref() {
                    Some(archives) => crate::platform::marker_archive_presence(
                        &self.reads,
                        archives,
                        &archive,
                        &format!("{}-{raw}-captured", marker.sweep_id),
                        |bytes| self.validate_committed_tree_bytes(marker, tree_hash, bytes),
                    )?,
                    None => (false, false),
                }
            };
            if canonical && (archived || captured) {
                return Err(corrupt("GC tree exists at canonical and archive paths"));
            }
            let present = canonical || archived || captured;
            any_missing |= !present;
            all_missing &= !present;
        }
        Ok((any_missing, all_missing))
    }
    fn validate_committed_tree_bytes(
        &self,
        marker: &GcInProgressMarker,
        tree_hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let expected = marker
            .candidates
            .iter()
            .find(|candidate| candidate.tree_hash == tree_hash)
            .map(|candidate| candidate.size_bytes)
            .ok_or_else(|| corrupt("marker tree has no candidate size"))?;
        if bytes.len() as u64 != expected {
            return Err(corrupt("GC tree length differs from frozen marker"));
        }
        validate_gc_tree_bytes(bytes, tree_hash)
    }
    /// Prove, using only the retained `.kio` capability, that the public index
    /// leaf names the latest durable state recorded by the marker: the final
    /// rotation when present, otherwise the mandatory pre-sweep rotation.
    /// Core does not interpret SQLite pages (that remains `kio-index`'s job),
    /// but it must not trust an unverified marker field at a deletion boundary.
    fn require_live_pre_sweep_index_binding(&self, marker: &GcInProgressMarker) -> Result<()> {
        let expected = marker
            .index_final
            .as_ref()
            .or(marker.index_pre_sweep.as_ref())
            .ok_or_else(|| corrupt("GC pre-sweep index state is missing"))?;
        let index = match open_optional_dir(&self.kio, "index")? {
            Some(index) => index,
            None if matches!(expected, GcIndexState::Absent) => return Ok(()),
            None => {
                return Err(corrupt(
                    "GC source index changed: index directory is missing",
                ));
            }
        };
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let leaf = Path::new("sqlite.db");
        let before = match cap_fs::stat(&index, leaf, cap_fs::FollowSymlinks::No) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(expected, GcIndexState::Absent) =>
            {
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(corrupt("GC source index changed: sqlite.db is missing"));
            }
            Err(error) => return Err(ioerr(error, "index/sqlite.db")),
        };
        valid_file(&before, u64::MAX)?;
        let file = match cap_fs::open(&index, Path::new("sqlite.db"), &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(corrupt("GC source index changed while opening"));
            }
            Err(error) => return Err(ioerr(error, "index/sqlite.db")),
        };
        let metadata =
            cap_fs::Metadata::from_file(&file).map_err(|error| ioerr(error, "index/sqlite.db"))?;
        valid_file(&metadata, u64::MAX)?;
        if !same_file_state(&before, &metadata)? {
            return Err(corrupt("GC source index changed while opening"));
        }
        let after = cap_fs::stat(&index, leaf, cap_fs::FollowSymlinks::No)
            .map_err(|error| ioerr(error, "index/sqlite.db"))?;
        valid_file(&after, u64::MAX)?;
        if !same_file_state(&after, &metadata)? {
            return Err(corrupt("GC source index changed while binding"));
        }
        let live = canonical_gc_index_identity_from_metadata(&metadata)?;
        let active_final_rotation_target = marker.index_rotation.as_ref().and_then(|rotation| {
            (rotation.role == GcIndexRotationRole::Final).then_some(&rotation.target)
        });
        match expected {
            GcIndexState::Absent => Err(corrupt(
                "GC marker records an absent pre-sweep index but sqlite.db exists",
            )),
            GcIndexState::Present { identity, .. }
                if identity == &live
                    || matches!(
                        active_final_rotation_target,
                        Some(GcIndexState::Present { identity, .. }) if identity == &live
                    ) =>
            {
                Ok(())
            }
            GcIndexState::Present { .. } => Err(corrupt("GC source index changed identity")),
        }
    }
    fn validate_tree_removal_permit(
        &self,
        permit: &GcTreeRemovalPermit,
        marker: &GcInProgressMarker,
    ) -> Result<()> {
        if permit.kio_identity != id_file(&self.kio)?
            || permit.marker_digest != hash_bytes(&marker.canonical_bytes()?)
        {
            return Err(corrupt("GC tree-removal permit does not bind marker/store"));
        }
        match (
            marker.index_pre_sweep.as_ref(),
            permit.index_file.as_ref(),
            permit.index_state.as_ref(),
        ) {
            (Some(GcIndexState::Absent), None, None) => Ok(()),
            (Some(GcIndexState::Present { identity, .. }), Some(file), Some(expected_state)) => {
                let metadata = cap_fs::Metadata::from_file(file)
                    .map_err(|error| ioerr(error, "index/sqlite.db"))?;
                valid_file(&metadata, u64::MAX)?;
                if canonical_gc_index_identity_from_metadata(&metadata)? != *identity
                    || file_state(&metadata) != *expected_state
                {
                    return Err(corrupt("attested GC index changed before tree removal"));
                }
                Ok(())
            }
            _ => Err(corrupt("GC tree-removal permit index state is invalid")),
        }
    }
    fn recheck_tree_receipts(&self, marker: &GcInProgressMarker, tree_hash: &str) -> Result<()> {
        if let Some(expected) = &marker.operation_receipts_digest
            && &operation_receipt_observation_digest_bound(&self.kio, marker)? != expected
        {
            return Err(corrupt(
                "marker-owned receipt identity/content changed before tree removal",
            ));
        }
        let gc = open_required_dir(&self.kio, "gc", "GC directory is missing")?;
        let shallow = open_required_dir(&gc, "shallowed", "GC receipt directory is missing")?;
        for candidate in marker
            .candidates
            .iter()
            .filter(|candidate| candidate.tree_hash == tree_hash)
        {
            let leaf = &candidate.commit_hash["sha256:".len()..];
            let (first, observed) = read_regular_observed(&shallow, leaf, MAX_METADATA)?;
            let receipt = ShallowReceipt::parse_canonical(&first, leaf)?;
            let expected = ShallowReceipt::new(
                candidate.commit_hash.clone(),
                candidate.tree_hash.clone(),
                marker.started_at.clone(),
            )?;
            if receipt != expected {
                return Err(corrupt("frozen receipt differs from candidate"));
            }
            let (second, recheck) = read_regular_observed(&shallow, leaf, MAX_METADATA)?;
            if observed != recheck || first != second {
                return Err(corrupt("frozen receipt changed before tree removal"));
            }
        }
        Ok(())
    }
}
