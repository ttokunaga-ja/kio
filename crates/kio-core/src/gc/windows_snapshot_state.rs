//! Windows scheduler checkpoint replacement with retained exchange authority.
use super::*;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use windows_exchange::{ExchangeExpected, OperationKind};

fn directory(file: &File) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        file.try_clone()
            .map_err(|e| ioerr(e, "snapshot state directory"))?,
        PathBuf::from("<retained snapshot state directory>"),
    )
}

fn observe(file: &File) -> Result<(SnapshotAutoState, FileObservation)> {
    let before = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "snapshot state"))?;
    valid_file(&before, MAX_METADATA)?;
    let mut cursor = file;
    cursor
        .seek(SeekFrom::Start(0))
        .map_err(|e| ioerr(e, "snapshot state"))?;
    let mut bytes = Vec::new();
    cursor
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ioerr(e, "snapshot state"))?;
    let after = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "snapshot state"))?;
    valid_file(&after, MAX_METADATA)?;
    if !same_file_state(&before, &after)? || bytes.len() as u64 != before.len() {
        return Err(snapshot_auto_state_changed());
    }
    Ok((
        parse_snapshot_auto_state(&bytes)?,
        FileObservation {
            identity: id_meta(&after)?,
            state: file_state(&after),
            digest: hash_bytes(&bytes),
        },
    ))
}

fn validate_handles(source: Option<&File>, target: &File) -> Result<()> {
    let (new, _) = observe(target)?;
    if let Some(source) = source {
        validate_snapshot_state_transition(&observe(source)?.0, &new)?;
    }
    Ok(())
}

impl GcSweepSession {
    fn snapshot_exchange_authority(
        &self,
        snapshot: &SnapshotAutoBinding,
        gc: &GcAutomationBinding,
    ) -> Result<(String, FileObservation, FileObservation)> {
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
        Ok((binding[7..].to_owned(), head_observation, tool_observation))
    }

    pub(super) fn replace_snapshot_state_windows(
        &self,
        temporary: &str,
        bytes: &[u8],
        expected: &FileObservation,
    ) -> Result<()> {
        if !is_snapshot_auto_state_temporary_name(temporary) {
            return Err(corrupt("invalid snapshot state temporary"));
        }
        let snapshot = self.snapshot_auto_binding()?;
        let gc_binding = self.automation_binding()?;
        let binding = self.snapshot_exchange_authority(&snapshot, &gc_binding)?;
        let gc = ensure_child_dir(&self.kio, "gc")?;
        let internal = ensure_child_dir(&gc, "internal")?;
        let owner = directory(&ensure_child_dir(&internal, "snapshot-state-exchange")?)?;
        let parent = directory(&self.kio)?;
        let target_state = parse_snapshot_auto_state(bytes)?;
        let authority = {
            let source =
                parent.open_gc_mutation(Path::new(SNAPSHOT_AUTO_STATE_LEAF), MAX_METADATA)?;
            if observe(&source)?.1 != *expected {
                return Err(snapshot_auto_state_changed());
            }
            let target = parent.open_gc_mutation(Path::new(temporary), MAX_METADATA)?;
            if observe(&target)?.0 != target_state {
                return Err(snapshot_auto_state_changed());
            }
            ExchangeExpected {
                source: windows_exchange::observe(&source, MAX_METADATA)?,
                target: windows_exchange::observe(&target, MAX_METADATA)?,
            }
        };
        let pending = windows_exchange::begin(
            &owner,
            &parent,
            SNAPSHOT_AUTO_STATE_LEAF,
            &parent,
            temporary,
            OperationKind::SnapshotState,
            &authority,
            MAX_METADATA,
            &binding.0,
            |source, target| {
                if observe(source)?.1 != *expected || observe(target)?.0 != target_state {
                    return Err(snapshot_auto_state_changed());
                }
                if self.snapshot_exchange_authority(&snapshot, &gc_binding)? != binding {
                    return Err(snapshot_auto_authority_changed());
                }
                validate_handles(Some(source), target)
            },
        )?;
        pending.complete(&authority, |_| Ok(()))?;
        if self.snapshot_exchange_authority(&snapshot, &gc_binding)? != binding {
            return Err(snapshot_auto_authority_changed());
        }
        Ok(())
    }

    pub(super) fn recover_snapshot_state_windows(
        &self,
        snapshot: &SnapshotAutoBinding,
        gc_binding: &GcAutomationBinding,
    ) -> Result<()> {
        if !has_pending_snapshot_state_exchange(&self.kio)? {
            return Ok(());
        }
        let binding = self.snapshot_exchange_authority(snapshot, gc_binding)?;
        let gc = open_optional_dir(&self.kio, "gc")?
            .ok_or_else(|| corrupt("missing snapshot exchange GC parent"))?;
        let internal = open_optional_dir(&gc, "internal")?
            .ok_or_else(|| corrupt("missing snapshot exchange internal parent"))?;
        let owner_file = open_optional_dir(&internal, "snapshot-state-exchange")?
            .ok_or_else(|| corrupt("missing snapshot exchange owner"))?;
        let owner = directory(&owner_file)?;
        let parent = directory(&self.kio)?;
        windows_exchange::validate_pending_authority(&owner, &binding.0)?;
        // Atomic-only residue never explains an absent checkpoint: it represents
        // either unpublished intent boot or cleanup after exact source retirement.
        let stable_public = if !owner.contains_entry(Path::new("intent.json"))? {
            let (bytes, observation) =
                read_regular_observed(&self.kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA)?;
            parse_snapshot_auto_state(&bytes)?;
            Some(observation)
        } else {
            None
        };
        if let Some(pending) = windows_exchange::open_pending_under_lock(
            &owner,
            &parent,
            &parent,
            OperationKind::SnapshotState,
            MAX_METADATA,
            &binding.0,
        )? {
            validate_handles(pending.source(), pending.target())?;
            if self.snapshot_exchange_authority(snapshot, gc_binding)? != binding {
                return Err(snapshot_auto_authority_changed());
            }
            let expected = ExchangeExpected {
                source: pending.source_record().clone(),
                target: pending.target_record().clone(),
            };
            pending.complete(&expected, |_| Ok(()))?;
        } else if let Some(expected) = stable_public {
            if read_regular_observed(&self.kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA)?.1
                != expected
            {
                return Err(snapshot_auto_state_changed());
            }
        } else {
            return Err(corrupt(
                "snapshot exchange disappeared without terminal authority",
            ));
        }
        if self.snapshot_exchange_authority(snapshot, gc_binding)? != binding {
            return Err(snapshot_auto_authority_changed());
        }
        self.snapshot_auto_state()?;
        Ok(())
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::test_control::{DebugTestControl, GcFault, Selector, install_scoped};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, GcSweepSession, SnapshotAutoStateBinding) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let config_path = repo.canonical_root().join(".kio/config.toml");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str(
            "\n[snapshot.auto]\nenabled = true\ninterval_seconds = 60\non_change_threshold = 1\n",
        );
        fs::write(config_path, config).unwrap();
        let session = GcSweepSession::bind(repo.canonical_root().to_path_buf()).unwrap();
        let first = session
            .publish_snapshot_auto_state(
                &session.snapshot_auto_state().unwrap(),
                "2026-09-27T00:00:00Z",
                &format!("sha256:{}", "a".repeat(64)),
                true,
            )
            .unwrap();
        (temp, session, first)
    }

    fn interrupt(session: &GcSweepSession, first: &SnapshotAutoStateBinding, fault: GcFault) {
        let mut control = DebugTestControl::default();
        control.core.gc_fault = Selector::Known(fault);
        let _guard = install_scoped(control);
        let error = session
            .publish_snapshot_auto_state(
                first,
                "2026-09-27T00:01:00Z",
                &format!("sha256:{}", "b".repeat(64)),
                true,
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-GC-TEST-INTERRUPTED-001");
    }

    #[test]
    fn every_snapshot_exchange_fault_preserves_checkpoint_on_locked_resume() {
        for fault in [
            GcFault::AfterWindowsSnapshotStateExchangeIntent,
            GcFault::AfterWindowsSnapshotStateExchangeSourceCapture,
            GcFault::AfterWindowsSnapshotStateExchangeTargetPublish,
            GcFault::AfterWindowsSnapshotStateExchangeNamesComplete,
            GcFault::AfterWindowsSnapshotStateExchangeSourceRetire,
            GcFault::AfterWindowsSnapshotStateExchangeIntentRemove,
        ] {
            let (_temp, session, first) = fixture();
            interrupt(&session, &first, fault);
            if fault != GcFault::AfterWindowsSnapshotStateExchangeIntentRemove {
                assert!(session.snapshot_auto_state().is_err());
                assert!(ensure_no_active_sweep_bound(&session.kio).is_err());
            }
            let config = session.snapshot_auto_binding().unwrap();
            let gc = session.automation_binding().unwrap();
            let _lock = session.acquire_snapshot_auto_recovery_lock().unwrap();
            session
                .recover_snapshot_auto_state_under_lock(&config, &gc)
                .unwrap();
            let resumed = session.snapshot_auto_state().unwrap();
            assert_eq!(
                resumed.state.unwrap().last_successful_eligible_attempt_at,
                "2026-09-27T00:01:00Z"
            );
            assert!(!session.has_pending_snapshot_auto_state_exchange().unwrap());
            session
                .recover_snapshot_auto_state_under_lock(&config, &gc)
                .unwrap();
        }
    }

    #[test]
    fn changed_policy_refuses_replay_but_restored_bytes_allow_fresh_authority() {
        let (_temp, session, first) = fixture();
        interrupt(
            &session,
            &first,
            GcFault::AfterWindowsSnapshotStateExchangeSourceCapture,
        );
        let path = session.root.join(".kio/config.toml");
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed.extend_from_slice(b"\n# changed policy observation\n");
        fs::write(&path, changed).unwrap();
        let _lock = session.acquire_snapshot_auto_recovery_lock().unwrap();
        assert!(
            session
                .recover_snapshot_auto_state_under_lock(
                    &session.snapshot_auto_binding().unwrap(),
                    &session.automation_binding().unwrap()
                )
                .is_err()
        );
        assert!(session.has_pending_snapshot_auto_state_exchange().unwrap());
        fs::write(path, original).unwrap();
        session
            .recover_snapshot_auto_state_under_lock(
                &session.snapshot_auto_binding().unwrap(),
                &session.automation_binding().unwrap(),
            )
            .unwrap();
        assert!(session.snapshot_auto_state().unwrap().state.is_some());
    }
}
