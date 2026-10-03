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

fn open_snapshot_source(parent: &File, expected: &FileObservation) -> Result<File> {
    match directory(parent)?.open_gc_mutation(Path::new(SNAPSHOT_AUTO_STATE_LEAF), MAX_METADATA) {
        Ok(source) => Ok(source),
        Err(acquisition_error) => {
            // A replacement's live writer can deny the exclusive mutation pin
            // before observe() compares identity. Metadata under the retained
            // parent can prove stale CAS authority, but cannot authorize mutation.
            // Missing, unsafe, unreadable, or unchanged leaves retain the original
            // acquisition error; a failed re-observation is not proof of a race.
            let changed = (|| -> Result<bool> {
                let metadata = cap_fs::stat(
                    parent,
                    Path::new(SNAPSHOT_AUTO_STATE_LEAF),
                    cap_fs::FollowSymlinks::No,
                )
                .map_err(|error| ioerr(error, "snapshot state"))?;
                valid_file(&metadata, MAX_METADATA)?;
                Ok(id_meta(&metadata)? != expected.identity
                    || (metadata.modified().is_ok() && file_state(&metadata) != expected.state))
            })();
            if matches!(changed, Ok(true)) {
                Err(snapshot_auto_state_changed())
            } else {
                Err(acquisition_error)
            }
        }
    }
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
        temporary_observation: &FileObservation,
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
            let source = match open_snapshot_source(&self.kio, expected) {
                Ok(source) => source,
                Err(error) => {
                    // Only this pre-journal CAS conflict may retire our prepared
                    // target. Later failures leave journal-owned recovery state.
                    // Unknown or pending owner state never authorizes cleanup.
                    if error.error_code() == "KIO-E-SNAPSHOT-STATE-CHANGED-001"
                        && !windows_exchange::inspect_pending(&owner)?
                    {
                        retire_snapshot_state_temporary(
                            &self.kio,
                            temporary,
                            bytes,
                            temporary_observation,
                        )?;
                    }
                    return Err(error);
                }
            };
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
    fn changed_snapshot_source_with_live_writer_reports_state_changed() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let (_temp, session, first) = fixture();
        let path = session.root.join(".kio").join(SNAPSHOT_AUTO_STATE_LEAF);
        let original = fs::read(&path).unwrap();
        let before_head = fs::read(session.root.join(".kio/HEAD")).unwrap();
        let replacement = path.with_extension("competing-writer");
        let mut writer = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&replacement)
            .unwrap();
        // Identical bytes still represent a different object. Keep its writer
        // live through the acquisition and preservation assertions.
        writer.write_all(&original).unwrap();
        writer.sync_all().unwrap();
        fs::rename(&replacement, &path).unwrap();
        let error =
            open_snapshot_source(&session.kio, first.observation.as_ref().unwrap()).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SNAPSHOT-STATE-CHANGED-001");
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read(session.root.join(".kio/HEAD")).unwrap(),
            before_head
        );
        assert!(!session.has_pending_snapshot_auto_state_exchange().unwrap());
        drop(writer);
    }

    #[test]
    fn unchanged_snapshot_source_with_live_writer_preserves_acquisition_io() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let (_temp, session, first) = fixture();
        let path = session.root.join(".kio").join(SNAPSHOT_AUTO_STATE_LEAF);
        let original = fs::read(&path).unwrap();
        let writer = fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();
        let original_error = directory(&session.kio)
            .unwrap()
            .open_gc_mutation(Path::new(SNAPSHOT_AUTO_STATE_LEAF), MAX_METADATA)
            .unwrap_err();
        let error =
            open_snapshot_source(&session.kio, first.observation.as_ref().unwrap()).unwrap_err();
        assert_eq!(original_error.error_code(), "KIO-E-STORE-IO-001");
        assert_eq!(error.error_code(), original_error.error_code());
        assert_eq!(error.message(), original_error.message());
        assert_eq!(error.context(), original_error.context());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!session.has_pending_snapshot_auto_state_exchange().unwrap());
        drop(writer);
    }

    #[test]
    fn source_acquisition_conflict_retires_only_owned_prepared_temporary() {
        let (_temp, session, first) = fixture();
        let path = session.root.join(".kio").join(SNAPSHOT_AUTO_STATE_LEAF);
        let concurrent = fs::read(&path).unwrap();
        let replacement = path.with_extension("competing-writer");
        let mut writer = fs::File::create(&replacement).unwrap();
        writer.write_all(&concurrent).unwrap();
        writer.sync_all().unwrap();
        fs::rename(&replacement, &path).unwrap();
        let temporary = unique_internal_name(".snapshot-auto-state");
        create_new_bound(&session.kio, &temporary, &concurrent, MAX_METADATA).unwrap();
        let (_, temporary_observation) =
            read_regular_observed(&session.kio, &temporary, MAX_METADATA).unwrap();
        let error = session
            .replace_snapshot_state_windows(
                &temporary,
                &concurrent,
                first.observation.as_ref().unwrap(),
                &temporary_observation,
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SNAPSHOT-STATE-CHANGED-001");
        assert!(!session.root.join(".kio").join(&temporary).exists());
        assert_eq!(fs::read(&path).unwrap(), concurrent);
        assert!(!session.has_pending_snapshot_auto_state_exchange().unwrap());
        drop(writer);
    }

    #[test]
    fn pending_exchange_fault_retains_prepared_target_for_recovery() {
        let (_temp, session, first) = fixture();
        interrupt(
            &session,
            &first,
            GcFault::AfterWindowsSnapshotStateExchangeIntent,
        );
        assert!(session.has_pending_snapshot_auto_state_exchange().unwrap());
        let prepared = fs::read_dir(session.root.join(".kio"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".snapshot-auto-state-")
            })
            .collect::<Vec<_>>();
        assert_eq!(prepared.len(), 1);
        let target = parse_snapshot_auto_state(&fs::read(&prepared[0]).unwrap()).unwrap();
        assert_eq!(
            target.last_successful_eligible_attempt_at,
            "2026-09-27T00:01:00Z"
        );
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
