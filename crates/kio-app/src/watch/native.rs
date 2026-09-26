//! `notify` adapter. Its OS-specific backend is intentionally treated as a
//! source of lossy hints; queue overflow and backend errors always become a
//! root-wide reconciliation request.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use notify::event::{AccessKind, AccessMode};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::{DirtyReason, Reconcile, WatchEngine, WatchError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeWatcherStatus {
    Running,
    Degraded,
}

pub struct NativeWatcher<R: Reconcile> {
    _watcher: RecommendedWatcher,
    _engine: Arc<WatchEngine<R>>,
}

impl<R: Reconcile> NativeWatcher<R> {
    pub fn start(engine: Arc<WatchEngine<R>>) -> Result<Self, WatchError> {
        let callback_engine = Arc::clone(&engine);
        let mut watcher = RecommendedWatcher::new(
            move |event| handle_event(&callback_engine, event),
            Config::default(),
        )
        .map_err(|error| {
            engine.set_backend(
                "polling-required",
                true,
                Some(format!("native watcher initialization failed: {error}")),
            );
            WatchError::invariant(format!("cannot start native watcher: {error}"))
        })?;
        for root in engine.roots() {
            watcher
                .watch(&root.canonical_root, RecursiveMode::Recursive)
                .map_err(|error| {
                    engine.set_backend(
                        "polling-required",
                        true,
                        Some(format!("native watch registration failed: {error}")),
                    );
                    WatchError::invariant(format!(
                        "cannot watch {}: {error}",
                        root.canonical_root.display()
                    ))
                })?;
        }
        engine.set_backend("notify/recommended", false, None);
        Ok(Self {
            _watcher: watcher,
            _engine: engine,
        })
    }
}

fn handle_event<R: Reconcile>(engine: &WatchEngine<R>, event: notify::Result<Event>) {
    match event {
        Ok(event) => {
            if event.need_rescan() {
                enqueue_all(engine, DirtyReason::Overflow);
                return;
            }
            // Linux reports opens made by reconciliation itself, including
            // reads of policy controls. They do not change content or authority
            // and must not schedule another pass. Keep write-close and unknown
            // notifications conservative, and honor rescan flags above first.
            if matches!(
                event.kind,
                EventKind::Access(
                    AccessKind::Read | AccessKind::Open(_) | AccessKind::Close(AccessMode::Read)
                )
            ) {
                return;
            }
            if event.paths.is_empty() {
                enqueue_all(engine, DirtyReason::Overflow);
                return;
            }
            let mut paths_by_root = BTreeMap::<_, Vec<_>>::new();
            for path in event.paths {
                if let Some(root) = engine.root_for_path(&path) {
                    paths_by_root.entry(root).or_default().push(path);
                }
            }
            for (root, paths) in paths_by_root {
                // The callback has no authority to decide what changed;
                // enqueue_events performs root-relative validation before one
                // durable update for this backend notification.
                if let Err(error) = engine.enqueue_events(&root, &paths, DirtyReason::Native) {
                    engine.set_backend("notify/recommended", true, Some(error.to_string()));
                    let _ = engine.enqueue_event(&root, None, DirtyReason::Overflow);
                }
            }
        }
        Err(error) => {
            engine.set_backend("notify/recommended", true, Some(error.to_string()));
            enqueue_all(engine, DirtyReason::BackendError);
        }
    }
}

fn enqueue_all<R: Reconcile>(engine: &WatchEngine<R>, reason: DirtyReason) {
    for root in engine.roots() {
        let _ = engine.enqueue_event(&root, None, reason);
    }
}

#[allow(dead_code)]
fn _event_paths_are_hints(paths: &[PathBuf]) -> bool {
    !paths.is_empty()
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;
    use crate::watch::{ReconcileCompletion, WatchRoot};

    #[derive(Default)]
    struct RecordingReconciler(Mutex<Vec<Vec<PathBuf>>>);
    impl Reconcile for RecordingReconciler {
        fn reconcile(
            &self,
            _root: &WatchRoot,
            _reason: DirtyReason,
            changed_paths: &[PathBuf],
        ) -> Result<ReconcileCompletion, WatchError> {
            self.0.lock().unwrap().push(changed_paths.to_vec());
            Ok(ReconcileCompletion::Complete)
        }
    }

    #[test]
    fn read_notifications_do_not_dirty_controls_but_mutations_and_rescans_do() {
        use notify::event::{
            CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind, RenameMode,
        };

        let root_dir = tempdir().unwrap();
        let cases = [
            (EventKind::Access(AccessKind::Read), false),
            (EventKind::Access(AccessKind::Open(AccessMode::Any)), false),
            (
                EventKind::Access(AccessKind::Open(AccessMode::Write)),
                false,
            ),
            (
                EventKind::Access(AccessKind::Close(AccessMode::Read)),
                false,
            ),
            (
                EventKind::Access(AccessKind::Close(AccessMode::Write)),
                true,
            ),
            (EventKind::Access(AccessKind::Close(AccessMode::Any)), true),
            (EventKind::Modify(ModifyKind::Data(DataChange::Any)), true),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                true,
            ),
            (EventKind::Modify(ModifyKind::Name(RenameMode::Any)), true),
            (EventKind::Create(CreateKind::Any), true),
            (EventKind::Remove(RemoveKind::Any), true),
            (EventKind::Any, true),
            (EventKind::Other, true),
        ];
        for (kind, should_dirty) in cases {
            let queue_dir = tempdir().unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(queue_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            }
            let root = WatchRoot::new("event-test", root_dir.path().to_path_buf(), 1).unwrap();
            let engine = WatchEngine::open(
                &queue_dir.path().join("watch.sqlite"),
                [root],
                RecordingReconciler::default(),
            )
            .unwrap();
            assert!(engine.reconcile_once().unwrap());
            assert_eq!(engine.status().unwrap().backlog, 0);
            // Reading these controls used to request a full scan on every pass.
            for path in [".kioignore", ".kio/management.json", "document.md"] {
                handle_event(
                    &engine,
                    Ok(Event::new(kind).add_path(root_dir.path().join(path))),
                );
            }
            assert_eq!(
                engine.status().unwrap().backlog,
                usize::from(should_dirty),
                "{kind:?}"
            );
            if !should_dirty {
                handle_event(&engine, Ok(Event::new(kind)));
                assert_eq!(engine.status().unwrap().backlog, 0, "pathless {kind:?}");
                handle_event(&engine, Ok(Event::new(kind).set_flag(Flag::Rescan)));
                assert_eq!(engine.status().unwrap().backlog, 1, "rescan {kind:?}");
            }
        }
    }

    /// Each native CI host must exercise its selected notification backend.
    /// Running this on one host does not count as evidence for another OS.
    #[test]
    fn notify_reports_a_new_empty_directory() {
        let root_dir = tempdir().unwrap();
        let queue_dir = tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(queue_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root =
            WatchRoot::new("native-test", root_dir.path().canonicalize().unwrap(), 1).unwrap();
        let engine = Arc::new(
            WatchEngine::open(
                &queue_dir.path().join("watch.sqlite"),
                [root],
                RecordingReconciler::default(),
            )
            .unwrap(),
        );
        let _watcher = NativeWatcher::start(Arc::clone(&engine)).unwrap();
        // The startup full reconciliation is intentionally queued before the
        // watcher starts. Drain it before creating the directory so this test
        // proves that the native event, rather than startup, supplied the hint.
        assert!(engine.reconcile_once().unwrap());
        fs::create_dir(root_dir.path().join("new-empty")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let _ = engine.reconcile_once().unwrap();
            if engine
                .reconciler
                .0
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .any(|path| path == Path::new("new-empty"))
            {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("notify did not report the new empty directory before timeout");
    }
}
