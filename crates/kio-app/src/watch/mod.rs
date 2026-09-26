//! Reconciliation-oriented filesystem watching.
//!
//! Native events are only dirty hints. The caller supplies already verified
//! scope roots, and the reconciliation callback must reopen and re-authorize
//! every candidate before it changes any source-of-truth data.

mod native;
mod queue;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub use native::{NativeWatcher, NativeWatcherStatus};
pub use queue::{Claim, DirtyQueue, DirtyWork, EnqueueOutcome, QUEUE_CAPACITY};

pub const DEBOUNCE: Duration = Duration::from_millis(250);
pub const MAX_WAIT: Duration = Duration::from_secs(2);
pub const FULL_RECONCILE_INTERVAL: Duration = Duration::from_secs(300);
const CLAIM_LEASE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WatchRoot {
    /// Verified by the caller from the actual management registration. Event
    /// paths never supply a scope id, root, or authority generation.
    pub scope_id: String,
    pub canonical_root: PathBuf,
    pub generation: u64,
}

impl WatchRoot {
    pub fn new(
        scope_id: impl Into<String>,
        canonical_root: PathBuf,
        generation: u64,
    ) -> Result<Self, WatchError> {
        let scope_id = scope_id.into();
        if scope_id.is_empty() {
            return Err(WatchError::invariant("watch root has an empty scope id"));
        }
        if !canonical_root.is_absolute() {
            return Err(WatchError::invariant(
                "watch root must be canonical and absolute",
            ));
        }
        Ok(Self {
            scope_id,
            canonical_root,
            generation,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyReason {
    Native,
    Startup,
    Periodic,
    Manual,
    Overflow,
    BackendError,
    Control,
}
impl DirtyReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Startup => "startup",
            Self::Periodic => "periodic",
            Self::Manual => "manual",
            Self::Overflow => "overflow",
            Self::BackendError => "backend_error",
            Self::Control => "control",
        }
    }
    pub(crate) fn parse(value: &str) -> Result<Self, WatchError> {
        match value {
            "native" => Ok(Self::Native),
            "startup" => Ok(Self::Startup),
            "periodic" => Ok(Self::Periodic),
            "manual" => Ok(Self::Manual),
            "overflow" => Ok(Self::Overflow),
            "backend_error" => Ok(Self::BackendError),
            "control" => Ok(Self::Control),
            _ => Err(WatchError::invariant("unknown durable dirty reason")),
        }
    }
    pub(crate) fn requires_full_scan(self) -> bool {
        matches!(
            self,
            Self::Startup
                | Self::Periodic
                | Self::Manual
                | Self::Overflow
                | Self::BackendError
                | Self::Control
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileCompletion {
    Complete,
    Incomplete,
}

pub trait Reconcile: Send + Sync + 'static {
    /// `changed_paths` are root-relative hints only. Implementations must reject
    /// paths outside `root`, symlinks/reparse points, and stale authority.
    fn reconcile(
        &self,
        root: &WatchRoot,
        reason: DirtyReason,
        changed_paths: &[PathBuf],
    ) -> Result<ReconcileCompletion, WatchError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchStatus {
    pub backend: String,
    pub backlog: usize,
    pub degraded: bool,
    pub last_success: Option<SystemTime>,
    pub last_failure: Option<String>,
}

#[derive(Debug)]
struct MutableStatus {
    backend: String,
    backend_degraded: bool,
    degraded: bool,
    last_success: Option<SystemTime>,
    last_failure: Option<String>,
}

pub struct WatchEngine<R: Reconcile> {
    queue: Mutex<DirtyQueue>,
    roots: BTreeMap<PathBuf, WatchRoot>,
    reconciler: Arc<R>,
    shutdown: Arc<AtomicBool>,
    status: Mutex<MutableStatus>,
    next_periodic: Mutex<Instant>,
    full_reconcile_interval: Duration,
}

impl<R: Reconcile> WatchEngine<R> {
    pub fn open(
        queue_path: &Path,
        roots: impl IntoIterator<Item = WatchRoot>,
        reconciler: R,
    ) -> Result<Self, WatchError> {
        Self::open_with_interval(queue_path, roots, reconciler, FULL_RECONCILE_INTERVAL)
    }

    pub fn open_with_interval(
        queue_path: &Path,
        roots: impl IntoIterator<Item = WatchRoot>,
        reconciler: R,
        interval: Duration,
    ) -> Result<Self, WatchError> {
        Self::open_with_queue(
            DirtyQueue::open_private(queue_path)?,
            roots,
            reconciler,
            interval,
        )
    }

    /// Open a Linux queue through a retained owner-private directory instead
    /// of re-resolving the directory's diagnostic pathname.
    #[cfg(target_os = "linux")]
    pub fn open_with_interval_private_directory(
        directory: &kio_core::store_dir::StoreDirectory,
        queue_leaf: &Path,
        roots: impl IntoIterator<Item = WatchRoot>,
        reconciler: R,
        interval: Duration,
    ) -> Result<Self, WatchError> {
        Self::open_with_queue(
            DirtyQueue::open_private_at(directory, queue_leaf)?,
            roots,
            reconciler,
            interval,
        )
    }

    fn open_with_queue(
        mut queue: DirtyQueue,
        roots: impl IntoIterator<Item = WatchRoot>,
        reconciler: R,
        interval: Duration,
    ) -> Result<Self, WatchError> {
        if interval.is_zero() || interval > Duration::from_secs(86400) {
            return Err(WatchError::invariant(
                "invalid full reconciliation interval",
            ));
        }
        let mut registered = BTreeMap::new();
        for root in roots {
            if registered
                .insert(root.canonical_root.clone(), root)
                .is_some()
            {
                return Err(WatchError::invariant("duplicate watch root registration"));
            }
        }
        // A restart is intentionally never treated as continuity proof.
        for root in registered.values() {
            queue.enqueue_full(root, DirtyReason::Startup)?;
        }
        Ok(Self {
            queue: Mutex::new(queue),
            roots: registered,
            reconciler: Arc::new(reconciler),
            shutdown: Arc::new(AtomicBool::new(false)),
            status: Mutex::new(MutableStatus {
                backend: "not-started".into(),
                backend_degraded: false,
                degraded: false,
                last_success: None,
                last_failure: None,
            }),
            next_periodic: Mutex::new(Instant::now() + interval),
            full_reconcile_interval: interval,
        })
    }
    pub fn shutdown_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release)
    }
    pub fn enqueue_event(
        &self,
        root: &WatchRoot,
        path: Option<&Path>,
        reason: DirtyReason,
    ) -> Result<EnqueueOutcome, WatchError> {
        self.ensure_registered(root)?;
        let Some(path) = path else {
            return self
                .queue
                .lock()
                .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
                .enqueue(root, reason, []);
        };
        let Some(path) = self.classify_path(root, path)? else {
            return Ok(EnqueueOutcome::Ignored);
        };
        self.queue
            .lock()
            .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
            .enqueue(root, reason, [path])
    }
    /// Persist one native notification's path hints in a single durable queue
    /// update.  A backend commonly delivers many related paths together;
    /// committing each one independently turns that one notification into
    /// avoidable synchronous SQLite traffic and can delay reconciliation long
    /// enough to hide later real changes.
    pub fn enqueue_events(
        &self,
        root: &WatchRoot,
        paths: &[PathBuf],
        reason: DirtyReason,
    ) -> Result<EnqueueOutcome, WatchError> {
        self.ensure_registered(root)?;
        if paths.is_empty() {
            return self
                .queue
                .lock()
                .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
                .enqueue(root, reason, []);
        }
        // Native backends own this vector. Never preallocate from its length:
        // retain at most the queue boundary plus the one entry that proves a
        // root-wide scan is necessary. Every later input is still normalized
        // below so overflow cannot bypass path validation.
        let mut classified = BTreeSet::new();
        let mut saw_classified = false;
        let mut full_scan = false;
        for path in paths {
            if let Some(path) = self.classify_path(root, path)? {
                let path = queue::normalize_relative(path)?;
                saw_classified = true;
                if !full_scan {
                    classified.insert(path);
                    if classified.len() > QUEUE_CAPACITY {
                        full_scan = true;
                        classified.clear();
                    }
                }
            }
        }
        // Generated Kio state can account for every path in a native event.
        // Preserve the old per-path behavior: that event is ignored rather
        // than being mistaken for an empty/root-wide notification.
        if !saw_classified {
            return Ok(EnqueueOutcome::Ignored);
        }
        if full_scan {
            // An empty relative path is the queue's explicit durable full-scan
            // marker. Retain the original reason; overflow is a capacity
            // collapse, not evidence that notify requested a rescan.
            return self
                .queue
                .lock()
                .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
                .enqueue(root, reason, [PathBuf::new()]);
        }
        self.queue
            .lock()
            .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
            .enqueue(root, reason, classified)
    }
    /// Runs a single ready claim. The caller can drive this from a service loop
    /// or tests; no background writer bypasses the same durable queue.
    pub fn reconcile_once(&self) -> Result<bool, WatchError> {
        self.enqueue_periodic_if_due()?;
        let claim = self
            .queue
            .lock()
            .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
            .claim_ready(SystemTime::now(), DEBOUNCE, MAX_WAIT, CLAIM_LEASE)?;
        let Some(claim) = claim else { return Ok(false) };
        if self.ensure_registered(&claim.work.root).is_err() {
            self.queue
                .lock()
                .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
                .discard(&claim)?;
            self.set_backend(
                "notify/recommended",
                true,
                Some("discarded stale unregistered durable watch work".into()),
            );
            return Ok(true);
        }
        let result = self.reconciler.reconcile(
            &claim.work.root,
            claim.work.reason,
            &claim.work.changed_paths,
        );
        let complete = matches!(result, Ok(ReconcileCompletion::Complete));
        self.queue
            .lock()
            .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
            .complete(&claim, complete)?;
        let mut status = self
            .status
            .lock()
            .map_err(|_| WatchError::invariant("watch status lock poisoned"))?;
        match result {
            Ok(ReconcileCompletion::Complete) => {
                status.last_success = Some(SystemTime::now());
                status.last_failure = None;
                status.degraded = status.backend_degraded;
            }
            Ok(ReconcileCompletion::Incomplete) => {
                status.degraded = true;
                status.last_failure = Some("reconciliation incomplete; retained for retry".into());
            }
            Err(error) => {
                status.degraded = true;
                status.last_failure = Some(error.to_string());
            }
        }
        Ok(true)
    }
    pub fn run(&self) -> Result<(), WatchError> {
        while !self.shutdown.load(Ordering::Acquire) {
            if !self.reconcile_once()? {
                thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    }
    pub fn status(&self) -> Result<WatchStatus, WatchError> {
        let status = self
            .status
            .lock()
            .map_err(|_| WatchError::invariant("watch status lock poisoned"))?;
        Ok(WatchStatus {
            backend: status.backend.clone(),
            backlog: self
                .queue
                .lock()
                .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?
                .pending_count()?,
            degraded: status.degraded,
            last_success: status.last_success,
            last_failure: status.last_failure.clone(),
        })
    }
    pub(crate) fn set_backend(
        &self,
        backend: impl Into<String>,
        degraded: bool,
        failure: Option<String>,
    ) {
        if let Ok(mut status) = self.status.lock() {
            status.backend = backend.into();
            status.backend_degraded = degraded;
            status.degraded = degraded;
            if failure.is_some() {
                status.last_failure = failure;
            }
        }
    }
    fn enqueue_periodic_if_due(&self) -> Result<(), WatchError> {
        let now = Instant::now();
        let mut due = self
            .next_periodic
            .lock()
            .map_err(|_| WatchError::invariant("watch periodic lock poisoned"))?;
        if now >= *due {
            let mut queue = self
                .queue
                .lock()
                .map_err(|_| WatchError::invariant("watch queue lock poisoned"))?;
            for root in self.roots.values() {
                queue.enqueue_full(root, DirtyReason::Periodic)?;
            }
            *due = now + self.full_reconcile_interval;
        }
        Ok(())
    }
    fn ensure_registered(&self, root: &WatchRoot) -> Result<(), WatchError> {
        match self.roots.get(&root.canonical_root) {
            Some(registered) if registered == root => Ok(()),
            _ => Err(WatchError::invariant(
                "event root is not a current verified watch root",
            )),
        }
    }
    pub(crate) fn roots(&self) -> Vec<WatchRoot> {
        self.roots.values().cloned().collect()
    }
    pub(crate) fn root_for_path(&self, path: &Path) -> Option<WatchRoot> {
        self.roots
            .iter()
            .filter(|(candidate, _)| path.starts_with(candidate))
            .max_by_key(|(candidate, _)| candidate.components().count())
            .map(|(_, root)| root.clone())
    }
    fn classify_path(&self, root: &WatchRoot, path: &Path) -> Result<Option<PathBuf>, WatchError> {
        let relative = path
            .strip_prefix(&root.canonical_root)
            .map_err(|_| WatchError::invariant("native event path is outside registered root"))?
            .to_path_buf();
        if is_generated_kio_path(&relative) {
            Ok(None)
        } else if is_control_path(&relative) {
            // An empty relative hint is deliberately collapsed by the queue to
            // a root-wide scan: policy/consent changes invalidate more than the
            // one control file that happened to notify.
            Ok(Some(PathBuf::new()))
        } else {
            Ok(Some(relative))
        }
    }
}

/// Generated state is suppressed, but management and consent controls always
/// dirty their root. This avoids a self-event loop without hiding policy edits.
pub(crate) fn is_generated_kio_path(relative: &Path) -> bool {
    let parts = relative
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts
        .iter()
        .position(|part| *part == ".kio")
        .and_then(|position| parts.get(position + 1))
        .is_some_and(|_| !is_control_path(relative))
}

pub(crate) fn is_control_path(relative: &Path) -> bool {
    let parts = relative
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts.contains(&".kioignore")
        || parts
            .iter()
            .position(|part| *part == ".kio")
            .and_then(|position| parts.get(position + 1))
            .is_some_and(|kind| {
                matches!(
                    *kind,
                    "config.toml"
                        | "scope.json"
                        | "management.json"
                        | "consent.json"
                        | "consents.jsonl"
                        | "approvals"
                        | "approvals.jsonl"
                        | "tools.toml"
                        | "tool.lock"
                )
            })
}

#[derive(Debug, Clone)]
pub struct WatchError(String);
impl WatchError {
    pub(crate) fn invariant(message: impl Into<String>) -> Self {
        Self(message.into())
    }
    pub(crate) fn sqlite(error: rusqlite::Error) -> Self {
        Self(format!("watch queue sqlite error: {error}"))
    }
    pub(crate) fn json(error: serde_json::Error) -> Self {
        Self(format!("watch queue JSON error: {error}"))
    }
    pub(crate) fn io(error: std::io::Error) -> Self {
        Self(format!("watch queue IO error: {error}"))
    }
}
impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for WatchError {}

#[cfg(test)]
mod tests {
    use super::*;
    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }
    #[test]
    fn generated_self_events_do_not_hide_controls() {
        assert!(is_generated_kio_path(Path::new(".kio/index/sqlite.db")));
        for generated in [
            "HEAD",
            "manifest.json",
            ".lock",
            "tasks/pending.json",
            "refs/tags/latest",
        ] {
            assert!(is_generated_kio_path(&Path::new(".kio").join(generated)));
        }
        assert!(!is_generated_kio_path(Path::new(".kio/config.toml")));
        assert!(!is_generated_kio_path(Path::new(".kio/consent.json")));
        assert!(!is_generated_kio_path(Path::new(".kioignore")));
        assert!(is_control_path(Path::new(".kio/config.toml")));
        assert!(is_control_path(Path::new(".kioignore")));
        assert!(is_generated_kio_path(Path::new(
            "child/.kio/index/sqlite.db"
        )));
        assert!(is_control_path(Path::new(
            "child/.kio/approvals/grant.json"
        )));
    }
    #[test]
    fn ignored_event_does_not_create_work() {
        let dir = private_dir();
        let root_dir = tempfile::tempdir().unwrap();
        let root = WatchRoot::new("scope", root_dir.path().canonicalize().unwrap(), 1).unwrap();
        struct Complete;
        impl Reconcile for Complete {
            fn reconcile(
                &self,
                _: &WatchRoot,
                _: DirtyReason,
                _: &[PathBuf],
            ) -> Result<ReconcileCompletion, WatchError> {
                Ok(ReconcileCompletion::Complete)
            }
        }
        let engine =
            WatchEngine::open(&dir.path().join("queue.sqlite"), [root.clone()], Complete).unwrap();
        assert_eq!(
            engine
                .enqueue_event(
                    &root,
                    Some(&root.canonical_root.join("child/.kio/index/sqlite.db")),
                    DirtyReason::Native
                )
                .unwrap(),
            EnqueueOutcome::Ignored
        );
        assert_eq!(engine.status().unwrap().backlog, 1); // startup only
    }
    #[test]
    fn one_native_notification_batches_hints_and_ignores_generated_only_events() {
        let dir = private_dir();
        let root_dir = tempfile::tempdir().unwrap();
        let root = WatchRoot::new("scope", root_dir.path().canonicalize().unwrap(), 1).unwrap();
        #[derive(Default)]
        struct Complete(Mutex<Vec<Vec<PathBuf>>>);
        impl Reconcile for Complete {
            fn reconcile(
                &self,
                _: &WatchRoot,
                _: DirtyReason,
                paths: &[PathBuf],
            ) -> Result<ReconcileCompletion, WatchError> {
                self.0.lock().unwrap().push(paths.to_vec());
                Ok(ReconcileCompletion::Complete)
            }
        }
        let engine = WatchEngine::open(
            &dir.path().join("queue.sqlite"),
            [root.clone()],
            Complete::default(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(engine.reconcile_once().unwrap()); // startup
        assert_eq!(
            engine
                .enqueue_events(
                    &root,
                    &[
                        root.canonical_root.join("first.txt"),
                        root.canonical_root.join("second.txt"),
                        root.canonical_root.join(".kio/index/sqlite.db"),
                    ],
                    DirtyReason::Native,
                )
                .unwrap(),
            EnqueueOutcome::Queued
        );
        thread::sleep(Duration::from_millis(300));
        assert!(engine.reconcile_once().unwrap());
        assert_eq!(engine.status().unwrap().backlog, 0);
        assert_eq!(
            engine.reconciler.0.lock().unwrap().last().unwrap(),
            &vec![PathBuf::from("first.txt"), PathBuf::from("second.txt")]
        );
        assert_eq!(
            engine
                .enqueue_events(
                    &root,
                    &[root.canonical_root.join(".kio/index/sqlite.db")],
                    DirtyReason::Native,
                )
                .unwrap(),
            EnqueueOutcome::Ignored
        );
        assert_eq!(engine.status().unwrap().backlog, 0);
    }
    #[test]
    fn batch_hints_are_bounded_and_late_paths_still_validate() {
        let dir = private_dir();
        let root_dir = tempfile::tempdir().unwrap();
        let root = WatchRoot::new("scope", root_dir.path().canonicalize().unwrap(), 1).unwrap();
        #[derive(Default)]
        struct Recording(Mutex<Vec<Vec<PathBuf>>>);
        impl Reconcile for Recording {
            fn reconcile(
                &self,
                _: &WatchRoot,
                _: DirtyReason,
                paths: &[PathBuf],
            ) -> Result<ReconcileCompletion, WatchError> {
                self.0.lock().unwrap().push(paths.to_vec());
                Ok(ReconcileCompletion::Complete)
            }
        }
        let engine = WatchEngine::open(
            &dir.path().join("queue.sqlite"),
            [root.clone()],
            Recording::default(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(engine.reconcile_once().unwrap()); // startup

        let duplicates = vec![root.canonical_root.join("same.txt"); QUEUE_CAPACITY * 2];
        assert_eq!(
            engine
                .enqueue_events(&root, &duplicates, DirtyReason::Native)
                .unwrap(),
            EnqueueOutcome::Queued
        );
        thread::sleep(Duration::from_millis(300));
        assert!(engine.reconcile_once().unwrap());
        assert_eq!(
            engine.reconciler.0.lock().unwrap().last().unwrap(),
            &vec![PathBuf::from("same.txt")]
        );

        let oversized = (0..=QUEUE_CAPACITY)
            .map(|index| root.canonical_root.join(format!("hint-{index}")))
            .collect::<Vec<_>>();
        assert_eq!(
            engine
                .enqueue_events(&root, &oversized, DirtyReason::Native)
                .unwrap(),
            EnqueueOutcome::CollapsedToFullScan
        );
        thread::sleep(Duration::from_millis(300));
        assert!(engine.reconcile_once().unwrap());
        assert!(
            engine
                .reconciler
                .0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .is_empty()
        );

        let mut oversized_with_late_escape = oversized;
        oversized_with_late_escape.push(root.canonical_root.join("../outside.txt"));
        assert!(
            engine
                .enqueue_events(&root, &oversized_with_late_escape, DirtyReason::Native)
                .is_err()
        );
        assert_eq!(engine.status().unwrap().backlog, 0);
    }
    #[test]
    fn durable_work_for_an_unregistered_root_is_never_reconciled() {
        let dir = private_dir();
        let root_dir = tempfile::tempdir().unwrap();
        let root = WatchRoot::new("current", root_dir.path().canonicalize().unwrap(), 1).unwrap();
        #[derive(Default)]
        struct Calls(Mutex<Vec<String>>);
        impl Reconcile for Calls {
            fn reconcile(
                &self,
                root: &WatchRoot,
                _: DirtyReason,
                _: &[PathBuf],
            ) -> Result<ReconcileCompletion, WatchError> {
                self.0.lock().unwrap().push(root.scope_id.clone());
                Ok(ReconcileCompletion::Complete)
            }
        }
        let engine =
            WatchEngine::open(&dir.path().join("queue.sqlite"), [root], Calls::default()).unwrap();
        let stale = WatchRoot::new("stale", PathBuf::from("/private/stale-root"), 9).unwrap();
        engine
            .queue
            .lock()
            .unwrap()
            .enqueue_full(&stale, DirtyReason::Manual)
            .unwrap();
        engine.reconcile_once().unwrap();
        engine.reconcile_once().unwrap();
        assert!(
            !engine
                .reconciler
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == "stale")
        );
    }
}
