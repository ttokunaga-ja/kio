use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use kio_core::cas::read_bounded_regular_file;
use kio_core::{KioError, Result};

pub(crate) const DEFAULT_PARALLELISM: usize = 4;
pub(crate) const MAX_PARALLELISM: usize = 4;
pub(crate) const DEFAULT_PER_SCOPE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[cfg(debug_assertions)]
#[derive(Clone, Default)]
pub(crate) struct ScopeDelayTestControl {
    scope_id: Option<String>,
    delay_ms: Option<u64>,
}

#[cfg(debug_assertions)]
impl ScopeDelayTestControl {
    pub(crate) fn new(scope_id: Option<String>, delay_ms: Option<u64>) -> Self {
        Self { scope_id, delay_ms }
    }
}

#[cfg(not(debug_assertions))]
#[derive(Clone, Default)]
pub(crate) struct ScopeDelayTestControl;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MultiScopeSettings {
    pub(crate) parallelism: usize,
    pub(crate) per_scope_timeout: Duration,
}

impl MultiScopeSettings {
    pub(crate) fn new(parallelism: usize, per_scope_timeout: Duration) -> Self {
        Self {
            parallelism: parallelism.clamp(1, MAX_PARALLELISM),
            per_scope_timeout,
        }
    }
}

impl Default for MultiScopeSettings {
    fn default() -> Self {
        Self::new(DEFAULT_PARALLELISM, DEFAULT_PER_SCOPE_TIMEOUT)
    }
}

#[derive(Default)]
struct MultiScopeTuning {
    parallelism: Option<u64>,
    per_scope_timeout_seconds: Option<u64>,
}

fn read_tuning(path: &Path) -> Result<MultiScopeTuning> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MultiScopeTuning::default());
        }
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
        Ok(_) => {}
    };
    let text = String::from_utf8(read_bounded_regular_file(path, MAX_CONFIG_BYTES)?)
        .map_err(|error| KioError::schema(error.to_string()))?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| KioError::schema(error.to_string()))?;
    let multi_scope = value
        .get("search")
        .and_then(|search| search.get("multi_scope"));
    let integer = |name| {
        multi_scope
            .and_then(|section| section.get(name))
            .and_then(toml::Value::as_integer)
            .and_then(|value| u64::try_from(value).ok())
    };
    Ok(MultiScopeTuning {
        parallelism: integer("parallelism"),
        per_scope_timeout_seconds: integer("per_scope_timeout_seconds"),
    })
}

/// Resolve settings per key with scope config taking precedence over the user
/// config. Repository/user config validation has already enforced their schemas;
/// the checks here defend direct callers and Instant's platform-specific range.
pub(crate) fn effective_settings(
    scope_config: &Path,
    user_config: &Path,
) -> Result<MultiScopeSettings> {
    let scope = read_tuning(scope_config)?;
    let user = read_tuning(user_config)?;
    let parallelism = scope
        .parallelism
        .or(user.parallelism)
        .unwrap_or(DEFAULT_PARALLELISM as u64);
    if !(1..=MAX_PARALLELISM as u64).contains(&parallelism) {
        return Err(KioError::schema(format!(
            "search.multi_scope.parallelism must be between 1 and {MAX_PARALLELISM}"
        )));
    }
    let timeout_seconds = scope
        .per_scope_timeout_seconds
        .or(user.per_scope_timeout_seconds)
        .unwrap_or(DEFAULT_PER_SCOPE_TIMEOUT.as_secs());
    if timeout_seconds == 0 {
        return Err(KioError::schema(
            "search.multi_scope.per_scope_timeout_seconds must be at least 1",
        ));
    }
    let timeout = Duration::from_secs(timeout_seconds);
    if Instant::now().checked_add(timeout).is_none() {
        return Err(KioError::schema(
            "search.multi_scope.per_scope_timeout_seconds is too large for this platform",
        ));
    }
    Ok(MultiScopeSettings::new(parallelism as usize, timeout))
}

/// A deadline owned by one scope execution. Queue time is deliberately excluded:
/// the clock starts only after a worker claims the scope.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScopeDeadline {
    expires_at: Instant,
}

impl ScopeDeadline {
    fn from_now(timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            // Effective config validates this first. The fallback keeps the runner
            // fail-closed if a caller constructs an unrepresentable duration.
            expires_at: now.checked_add(timeout).unwrap_or(now),
        }
    }

    pub(crate) fn is_expired(self) -> bool {
        Instant::now() >= self.expires_at
    }

    #[cfg(debug_assertions)]
    fn remaining(self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ScopeExecution<T> {
    Completed(T),
    TimedOut,
}

/// Execute indexed jobs with a bounded scoped worker pool and return results in
/// input order, independent of completion order. Scoped threads are joined before
/// this function returns; no timeout path can leave detached work behind.
pub(crate) fn run_ordered<T, F>(
    job_count: usize,
    settings: MultiScopeSettings,
    run: F,
) -> Vec<ScopeExecution<T>>
where
    T: Send,
    F: Fn(usize, ScopeDeadline) -> T + Sync,
{
    if job_count == 0 {
        return Vec::new();
    }

    let worker_count = settings.parallelism.min(MAX_PARALLELISM).min(job_count);
    let next_job = AtomicUsize::new(0);
    let batches = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let run = &run;
            let next_job = &next_job;
            handles.push(scope.spawn(move || {
                let mut batch = Vec::new();
                loop {
                    let index = next_job.fetch_add(1, Ordering::Relaxed);
                    if index >= job_count {
                        break;
                    }
                    let deadline = ScopeDeadline::from_now(settings.per_scope_timeout);
                    let result = run(index, deadline);
                    let execution = if deadline.is_expired() {
                        ScopeExecution::TimedOut
                    } else {
                        ScopeExecution::Completed(result)
                    };
                    batch.push((index, execution));
                }
                batch
            }));
        }

        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(batch) => batch,
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect::<Vec<_>>()
    });

    let mut ordered = std::iter::repeat_with(|| None)
        .take(job_count)
        .collect::<Vec<_>>();
    for (index, execution) in batches.into_iter().flatten() {
        ordered[index] = Some(execution);
    }
    ordered
        .into_iter()
        .map(|entry| entry.expect("every claimed multi-scope job returns exactly once"))
        .collect()
}

/// Deterministic integration-test seam for completion-order and timeout tests.
/// It is unavailable in release builds and wakes in short intervals so the same
/// scope deadline still bounds the injected delay.
#[cfg(debug_assertions)]
pub(crate) fn maybe_delay_scope_for_test(
    scope_id: &str,
    deadline: ScopeDeadline,
    control: &ScopeDelayTestControl,
) {
    if control.scope_id.as_deref() != Some(scope_id) {
        return;
    }
    let Some(delay) = control.delay_ms.map(Duration::from_millis) else {
        return;
    };
    let started = Instant::now();
    loop {
        let elapsed = started.elapsed();
        if elapsed >= delay || deadline.is_expired() {
            return;
        }
        let delay_left = delay.saturating_sub(elapsed);
        let sleep_for = delay_left
            .min(deadline.remaining())
            .min(Duration::from_millis(10));
        if sleep_for.is_zero() {
            thread::yield_now();
        } else {
            thread::sleep(sleep_for);
        }
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn maybe_delay_scope_for_test(
    _scope_id: &str,
    _deadline: ScopeDeadline,
    _control: &ScopeDelayTestControl,
) {
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PARALLELISM, MultiScopeSettings, ScopeExecution, effective_settings, run_ordered,
    };
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    fn update_max(maximum: &AtomicUsize, value: usize) {
        let mut seen = maximum.load(Ordering::Relaxed);
        while value > seen {
            match maximum.compare_exchange_weak(seen, value, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => seen = actual,
            }
        }
    }

    #[test]
    fn results_keep_input_order_while_completion_order_reverses() {
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let settings = MultiScopeSettings::new(99, Duration::from_secs(1));
        let executions = run_ordered(8, settings, |index, _deadline| {
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&maximum, now_active);
            thread::sleep(Duration::from_millis(((8 - index) * 10) as u64));
            active.fetch_sub(1, Ordering::SeqCst);
            index
        });

        assert_eq!(maximum.load(Ordering::SeqCst), MAX_PARALLELISM);
        assert_eq!(
            executions,
            (0..8).map(ScopeExecution::Completed).collect::<Vec<_>>()
        );
    }

    #[test]
    fn timeout_is_classified_at_each_worker_completion() {
        let settings = MultiScopeSettings::new(2, Duration::from_millis(30));
        let executions = run_ordered(2, settings, |index, deadline| {
            if index == 0 {
                while !deadline.is_expired() {
                    thread::yield_now();
                }
            }
            index
        });
        assert_eq!(executions[0], ScopeExecution::TimedOut);
        assert_eq!(executions[1], ScopeExecution::Completed(1));
    }

    #[test]
    fn queued_job_receives_a_fresh_deadline_when_claimed() {
        // Inspect the per-job deadline directly instead of putting CI scheduler
        // jitter close to the timeout boundary. With one worker, each sleep is
        // queue time for the following job; its expiry must therefore move
        // forward rather than inherit a deadline created before it was claimed.
        let settings = MultiScopeSettings::new(1, Duration::from_secs(60));
        let executions = run_ordered(3, settings, |index, deadline| {
            if index < 2 {
                thread::sleep(Duration::from_millis(10));
            }
            (index, deadline.expires_at)
        });
        let completed = executions
            .into_iter()
            .map(|execution| match execution {
                ScopeExecution::Completed(value) => value,
                ScopeExecution::TimedOut => panic!("wide-margin queue test timed out"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            completed
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(completed[0].1 < completed[1].1);
        assert!(completed[1].1 < completed[2].1);
    }

    #[test]
    fn effective_settings_are_per_key_scope_over_user() {
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path().join("scope.toml");
        let user = dir.path().join("user.toml");
        fs::write(
            &scope,
            "[search.multi_scope]\nper_scope_timeout_seconds = 7\n",
        )
        .unwrap();
        fs::write(&user, "[search.multi_scope]\nparallelism = 2\n").unwrap();

        let settings = effective_settings(&scope, &user).unwrap();
        assert_eq!(settings.parallelism, 2);
        assert_eq!(settings.per_scope_timeout, Duration::from_secs(7));
    }

    #[test]
    fn oversized_config_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path().join("scope.toml");
        fs::write(&scope, vec![b' '; super::MAX_CONFIG_BYTES as usize + 1]).unwrap();
        let error = effective_settings(&scope, &dir.path().join("missing.toml")).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-OBJECT-OVERSIZED-001");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_fails_closed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.toml");
        let scope = dir.path().join("scope.toml");
        fs::write(&target, "[search.multi_scope]\nparallelism = 2\n").unwrap();
        symlink(&target, &scope).unwrap();
        let error = effective_settings(&scope, &dir.path().join("missing.toml")).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }
}
