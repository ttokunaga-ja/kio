//! Unix native notification subscription and lifetime.

use std::sync::Arc;

use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};

use crate::watch::{Reconcile, WatchEngine, WatchError};

pub(crate) struct NativeWatchBackend {
    _watcher: RecommendedWatcher,
}

pub(crate) fn watch_backend_name() -> &'static str {
    "notify/recommended"
}

impl NativeWatchBackend {
    pub(crate) fn start<R: Reconcile>(engine: Arc<WatchEngine<R>>) -> Result<Self, WatchError> {
        let callback_engine = Arc::clone(&engine);
        let mut watcher = RecommendedWatcher::new(
            move |event| crate::watch::native::handle_event(&callback_engine, event),
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
        engine.set_backend(watch_backend_name(), false, None);
        Ok(Self { _watcher: watcher })
    }
}
