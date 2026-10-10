//! Immutable controls owned by one invocation and its derived capabilities.

use crate::{Result, durability::DurabilityPoint, scope::append_observation};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default)]
pub struct ControlHandle {
    #[cfg(debug_assertions)]
    snapshot: std::sync::Arc<crate::test_control::DebugTestControl>,
}

impl ControlHandle {
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn new(snapshot: crate::test_control::DebugTestControl) -> Self {
        Self {
            snapshot: std::sync::Arc::new(snapshot),
        }
    }

    #[cfg(debug_assertions)]
    #[must_use]
    pub fn debug(&self) -> &crate::test_control::DebugTestControl {
        &self.snapshot
    }

    #[must_use]
    pub fn now_utc_seconds(&self) -> String {
        #[cfg(debug_assertions)]
        if let Some(value) = self.debug().fixed_now.known() {
            return value.clone();
        }
        crate::scope::now_utc_seconds()
    }

    #[must_use]
    pub fn now_millis(&self) -> i64 {
        #[cfg(debug_assertions)]
        if let Some(value) = self.debug().fixed_now.known()
            && value.len() == 20
            && let Some(seconds) = crate::scope::parse_utc_seconds(value)
        {
            return seconds * 1000;
        }
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    #[must_use]
    pub fn home_dir(&self) -> Option<PathBuf> {
        crate::xdg::home_dir_with_control(self)
    }

    pub fn checkpoint(&self, point: DurabilityPoint) -> Result<()> {
        crate::durability::checkpoint_with_control(self, point)
    }

    pub fn append_event_log(
        &self,
        code: &str,
        message: &str,
        context: serde_json::Value,
    ) -> Result<()> {
        append_observation(self, "events.jsonl", "info", code, message, context)
    }

    pub fn append_warn_log(
        &self,
        code: &str,
        message: &str,
        context: serde_json::Value,
    ) -> Result<()> {
        append_observation(self, "errors.jsonl", "warn", code, message, context)
    }

    pub fn append_error_log(&self, error: &crate::KioError) -> Result<()> {
        append_observation(
            self,
            "errors.jsonl",
            "error",
            error.error_code(),
            error.message(),
            error.context().clone(),
        )
    }

    pub fn append_jsonl_rotating(
        &self,
        path: &std::path::Path,
        value: &serde_json::Value,
        retention_days: u32,
    ) -> Result<()> {
        crate::scope::append_jsonl_rotating_with_control(self, path, value, retention_days)
    }
}
