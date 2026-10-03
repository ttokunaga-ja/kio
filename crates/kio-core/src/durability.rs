//! Debug-only process-stop barriers at durable state transitions.
//!
//! The public API deliberately remains available in release builds, where it
//! is a no-op and does not inspect process environment.

use crate::Result;

/// A named durable state transition that native process-stop tests may select.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurabilityPoint {
    PublicationJournal,
    PublicationManifest,
    PublicationHead,
    RestoreJournal,
    RestoreWorkingBytes,
    RestoreHead,
    ReplicaPublished,
    QueueClaimed,
    QueueCompleted,
    LedgerCheckpoint,
    LedgerWriteCommitted,
    LedgerBackupProof,
    LedgerRestoreJournal,
    LedgerRestoreDatabase,
    AtomicWriteStaged,
    AtomicWritePublished,
    AtomicRemoveReady,
    AtomicRemoveQuarantined,
    AtomicRemoveDeleted,
    CasRemoveReady,
    CasRemoveQuarantined,
    CasRemoveDeleted,
    PurgeCacheDeleted,
}

impl DurabilityPoint {
    #[cfg(debug_assertions)]
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "publication_journal" => Some(Self::PublicationJournal),
            "publication_manifest" => Some(Self::PublicationManifest),
            "publication_head" => Some(Self::PublicationHead),
            "restore_journal" => Some(Self::RestoreJournal),
            "restore_working_bytes" => Some(Self::RestoreWorkingBytes),
            "restore_head" => Some(Self::RestoreHead),
            "replica_published" => Some(Self::ReplicaPublished),
            "queue_claimed" => Some(Self::QueueClaimed),
            "queue_completed" => Some(Self::QueueCompleted),
            "ledger_checkpoint" => Some(Self::LedgerCheckpoint),
            "ledger_write_committed" => Some(Self::LedgerWriteCommitted),
            "ledger_backup_proof" => Some(Self::LedgerBackupProof),
            "ledger_restore_journal" => Some(Self::LedgerRestoreJournal),
            "ledger_restore_database" => Some(Self::LedgerRestoreDatabase),
            "atomic_write_staged" => Some(Self::AtomicWriteStaged),
            "atomic_write_published" => Some(Self::AtomicWritePublished),
            "atomic_remove_ready" => Some(Self::AtomicRemoveReady),
            "atomic_remove_quarantined" => Some(Self::AtomicRemoveQuarantined),
            "atomic_remove_deleted" => Some(Self::AtomicRemoveDeleted),
            "cas_remove_ready" => Some(Self::CasRemoveReady),
            "cas_remove_quarantined" => Some(Self::CasRemoveQuarantined),
            "cas_remove_deleted" => Some(Self::CasRemoveDeleted),
            "purge_cache_deleted" => Some(Self::PurgeCacheDeleted),
            _ => None,
        }
    }

    #[cfg(debug_assertions)]
    fn name(self) -> &'static str {
        match self {
            Self::PublicationJournal => "publication_journal",
            Self::PublicationManifest => "publication_manifest",
            Self::PublicationHead => "publication_head",
            Self::RestoreJournal => "restore_journal",
            Self::RestoreWorkingBytes => "restore_working_bytes",
            Self::RestoreHead => "restore_head",
            Self::ReplicaPublished => "replica_published",
            Self::QueueClaimed => "queue_claimed",
            Self::QueueCompleted => "queue_completed",
            Self::LedgerCheckpoint => "ledger_checkpoint",
            Self::LedgerWriteCommitted => "ledger_write_committed",
            Self::LedgerBackupProof => "ledger_backup_proof",
            Self::LedgerRestoreJournal => "ledger_restore_journal",
            Self::LedgerRestoreDatabase => "ledger_restore_database",
            Self::AtomicWriteStaged => "atomic_write_staged",
            Self::AtomicWritePublished => "atomic_write_published",
            Self::AtomicRemoveReady => "atomic_remove_ready",
            Self::AtomicRemoveQuarantined => "atomic_remove_quarantined",
            Self::AtomicRemoveDeleted => "atomic_remove_deleted",
            Self::CasRemoveReady => "cas_remove_ready",
            Self::CasRemoveQuarantined => "cas_remove_quarantined",
            Self::CasRemoveDeleted => "cas_remove_deleted",
            Self::PurgeCacheDeleted => "purge_cache_deleted",
        }
    }
}

/// Stop at a selected durable transition in debug builds.
///
/// A command composition root snapshots both controls once.  The marker is
/// created only when its exact typed selector is installed, then waits for an
/// adjacent `.release` marker for at most 30 seconds.  A production build is
/// intentionally inert.
pub fn checkpoint(point: DurabilityPoint) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        checkpoint_debug(point)
    }

    #[cfg(not(debug_assertions))]
    {
        let _ = point;
        Ok(())
    }
}

#[cfg(debug_assertions)]
fn checkpoint_debug(point: DurabilityPoint) -> Result<()> {
    use std::time::{Duration, Instant};

    let control = crate::test_control::current_or_default().core;
    if control.durability_point.known() != Some(&point) {
        return Ok(());
    }
    let Some(ready_path) = control.durability_ready else {
        return Ok(());
    };

    let payload = format!("point={}\npid={}\n", point.name(), std::process::id());
    publish_ready_marker_with(&ready_path, payload.as_bytes(), |_| Ok(())).map_err(|error| {
        crate::KioError::io(error.to_string(), ready_path.display().to_string())
    })?;

    let release_path = ready_path.with_extension("release");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match release_path.try_exists() {
            Ok(true) => return Ok(()),
            Ok(false) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(false) => {
                return Err(crate::KioError::new(
                    "KIO-E-TEST-DURABILITY-BARRIER-TIMEOUT-001",
                    "durability test barrier timed out waiting for release marker",
                    serde_json::json!({
                        "point": point.name(),
                        "ready": ready_path,
                        "release": release_path,
                    }),
                    crate::ExitCode::Failure,
                ));
            }
            Err(error) => {
                return Err(crate::KioError::io(
                    error.to_string(),
                    release_path.display().to_string(),
                ));
            }
        }
    }
}

/// Stage a complete marker next to its destination, then publish the name
/// without replacing an existing ready marker. The callback lets the unit test
/// observe the boundary after payload sync and before publication.
#[cfg(debug_assertions)]
fn publish_ready_marker_with(
    ready_path: &std::path::Path,
    payload: &[u8],
    before_publish: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::{Error, ErrorKind, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PENDING: AtomicU64 = AtomicU64::new(0);
    let basename = ready_path
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "ready marker has no basename"))?;
    let mut pending_name = basename.to_os_string();
    pending_name.push(format!(
        ".pending-{}-{}",
        std::process::id(),
        NEXT_PENDING.fetch_add(1, Ordering::Relaxed)
    ));
    let pending_path = ready_path.with_file_name(pending_name);
    let mut pending = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending_path)?;

    let publication = (|| {
        pending.write_all(payload)?;
        pending.sync_all()?;
        // Windows may reject hard-link creation while the source is open.
        drop(pending);
        before_publish(&pending_path)?;
        fs::hard_link(&pending_path, ready_path)
    })();

    // This process owns only the sibling it created. In particular, never
    // remove an existing ready marker if publication failed with AlreadyExists.
    let cleanup = fs::remove_file(&pending_path);
    match (publication, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => Err(Error::new(
            cleanup_error.kind(),
            format!(
                "ready marker was published but could not remove pending marker {}: {cleanup_error}",
                pending_path.display()
            ),
        )),
        (Err(error), Err(cleanup_error)) => Err(Error::new(
            error.kind(),
            format!(
                "ready marker publication failed: {error}; could not remove pending marker {}: {cleanup_error}",
                pending_path.display()
            ),
        )),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::{DurabilityPoint, checkpoint, publish_ready_marker_with};
    use crate::test_control::{CoreTestControl, DebugTestControl, Selector, install_scoped};
    use std::time::{Duration, Instant};

    #[test]
    fn absent_control_is_inert() {
        let _control = install_scoped(DebugTestControl::default());
        checkpoint(DurabilityPoint::PublicationJournal).unwrap();
    }

    #[test]
    fn selector_must_match_the_exact_point() {
        let _control = install_scoped(DebugTestControl {
            core: CoreTestControl {
                durability_point: Selector::Known(DurabilityPoint::PublicationHead),
                ..CoreTestControl::default()
            },
            ..DebugTestControl::default()
        });
        checkpoint(DurabilityPoint::PublicationJournal).unwrap();
    }

    #[test]
    fn selected_point_creates_a_ready_marker_and_waits_for_release() {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let release = ready.with_extension("release");
        let _control = install_scoped(DebugTestControl {
            core: CoreTestControl {
                durability_point: Selector::Known(DurabilityPoint::PublicationJournal),
                durability_ready: Some(ready.clone()),
                ..CoreTestControl::default()
            },
            ..DebugTestControl::default()
        });
        let release_ready = ready.clone();
        let releaser = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !release_ready.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                release_ready.exists(),
                "checkpoint did not create its ready marker"
            );
            std::fs::write(release, b"release").unwrap();
        });

        checkpoint(DurabilityPoint::PublicationJournal).unwrap();
        releaser.join().unwrap();
        let marker = std::fs::read_to_string(ready).unwrap();
        assert!(marker.contains("point=publication_journal\n"));
        assert!(marker.contains("pid="));
    }

    #[test]
    fn ready_marker_is_published_only_after_complete_sync_and_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("manifest.ready");
        let payload = b"point=publication_manifest\npid=123\n";
        let mut pending_path = None;

        publish_ready_marker_with(&ready, payload, |pending| {
            pending_path = Some(pending.to_path_buf());
            assert!(!ready.exists(), "ready marker appeared before publication");
            assert_eq!(std::fs::read(pending)?.as_slice(), payload);
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&ready).unwrap().as_slice(), payload);
        assert!(!pending_path.unwrap().exists());

        let stale = b"point=stale\npid=456\n";
        std::fs::write(&ready, stale).unwrap();
        let mut second_pending_path = None;
        let error = publish_ready_marker_with(&ready, payload, |pending| {
            second_pending_path = Some(pending.to_path_buf());
            assert_eq!(std::fs::read(&ready)?.as_slice(), stale);
            assert_eq!(std::fs::read(pending)?.as_slice(), payload);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&ready).unwrap().as_slice(), stale);
        assert!(!second_pending_path.unwrap().exists());
    }
}
