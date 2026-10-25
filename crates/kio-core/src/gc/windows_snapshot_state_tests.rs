use super::*;

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::platform::gc_snapshot_io::{directory, open_snapshot_source};
    use crate::test_control::{DebugTestControl, GcFault, Selector};
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
        let session = GcSweepSession::bind_with_control(
            session.root.clone(),
            crate::control::ControlHandle::new(control),
        )
        .unwrap();
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
        let error = open_snapshot_source(
            &session.kio,
            first.observation.as_ref().unwrap(),
            session.control(),
        )
        .unwrap_err();
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
        let original_error = crate::platform::open_gc_mutation(
            &directory(&session.kio, session.control()).unwrap(),
            Path::new(SNAPSHOT_AUTO_STATE_LEAF),
            MAX_METADATA,
        )
        .unwrap_err();
        let error = open_snapshot_source(
            &session.kio,
            first.observation.as_ref().unwrap(),
            session.control(),
        )
        .unwrap_err();
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
            .replace_snapshot_state(
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
    fn observed_source_conflict_retires_only_owned_prepared_temporary() {
        let (_temp, session, first) = fixture();
        let path = session.root.join(".kio").join(SNAPSHOT_AUTO_STATE_LEAF);
        let concurrent = fs::read(&path).unwrap();
        let before_head = fs::read(session.root.join(".kio/HEAD")).unwrap();
        let replacement = "snapshot-auto.competing-closed-writer";
        // Use canonical checkpoint bytes and an owner-private file, with its
        // writer closed, so acquisition succeeds before identity rejects CAS.
        create_new_bound(&session.kio, replacement, &concurrent, MAX_METADATA).unwrap();
        fs::rename(session.root.join(".kio").join(replacement), &path).unwrap();
        let expected = first.observation.as_ref().unwrap();
        let source = open_snapshot_source(&session.kio, expected, session.control()).unwrap();
        let (state, observation) = crate::platform::gc_snapshot_io::observe(&source).unwrap();
        assert_eq!(state, parse_snapshot_auto_state(&concurrent).unwrap());
        assert_ne!(observation.identity, expected.identity);
        drop(source);

        let temporary = unique_internal_name(".snapshot-auto-state");
        create_new_bound(&session.kio, &temporary, &concurrent, MAX_METADATA).unwrap();
        let (_, temporary_observation) =
            read_regular_observed(&session.kio, &temporary, MAX_METADATA).unwrap();
        let error = session
            .replace_snapshot_state(&temporary, &concurrent, expected, &temporary_observation)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SNAPSHOT-STATE-CHANGED-001");
        assert_eq!(fs::read(&path).unwrap(), concurrent);
        assert_eq!(
            fs::read(session.root.join(".kio/HEAD")).unwrap(),
            before_head
        );
        assert!(!session.root.join(".kio").join(&temporary).exists());
        assert!(!session.has_pending_snapshot_auto_state_exchange().unwrap());
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
                assert!(ensure_no_active_sweep_bound(&session.kio, session.control()).is_err());
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
