//! OS confinement for document renderers.
//!
//! A bounded process group alone cannot stop a compromised renderer from
//! reading the invoking user's files or using the network.  This module
//! constructs a fail-closed OS boundary for the renderer's immutable binary,
//! explicitly selected runtime roots, and one private scratch directory.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::BoundedProcessError;
use crate::platform::{NativeRenderSandbox, default_render_resource_limits};
use crate::{BoundedProcessOptions, BoundedProcessOutput};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderResourceLimits {
    pub wall_timeout: Duration,
    pub cpu_seconds: u64,
    pub max_address_space_bytes: u64,
    /// Whole-invocation kernel memory limit on Linux and Windows.
    pub max_aggregate_memory_bytes: u64,
    /// macOS only: an observed physical-footprint threshold for the one
    /// renderer process. This is sampled periodically and is therefore not a
    /// kernel-enforced instantaneous memory limit.
    pub max_physical_memory_bytes: u64,
    pub max_file_bytes: u64,
}

impl Default for RenderResourceLimits {
    fn default() -> Self {
        default_render_resource_limits()
    }
}

#[derive(Debug, Error)]
pub enum ConfinementError {
    #[error("renderer program must resolve to an absolute regular file: {0}")]
    Program(PathBuf),
    #[error("renderer scratch directory must resolve to an absolute directory: {0}")]
    Scratch(PathBuf),
    #[error("renderer runtime root must resolve to an absolute path: {0}")]
    Runtime(PathBuf),
    #[error("required renderer confinement backend is unavailable: {0}")]
    BackendUnavailable(&'static str),
    #[error("could not create renderer confinement profile: {0}")]
    Profile(#[source] std::io::Error),
    #[error("could not configure renderer resource limits: {0}")]
    Resource(#[source] std::io::Error),
    #[error(transparent)]
    Process(#[from] BoundedProcessError),
}

/// Immutable renderer boundary. All path values are canonicalized before the
/// child is created, and no ambient home or filesystem root is admitted.
#[derive(Debug, Clone)]
pub struct RenderSandbox {
    limits: RenderResourceLimits,
    pub(crate) native: NativeRenderSandbox,
}

impl RenderSandbox {
    pub fn new(
        program: &Path,
        scratch: &Path,
        runtime_roots: impl IntoIterator<Item = PathBuf>,
        limits: RenderResourceLimits,
    ) -> Result<Self, ConfinementError> {
        Self::from_sources(
            &RenderSources::resolve(program, runtime_roots)?,
            scratch,
            limits,
        )
    }

    pub fn from_sources(
        sources: &RenderSources,
        scratch: &Path,
        limits: RenderResourceLimits,
    ) -> Result<Self, ConfinementError> {
        let scratch = canonical_directory(scratch)
            .ok_or_else(|| ConfinementError::Scratch(scratch.into()))?;
        Ok(Self {
            native: NativeRenderSandbox::new(sources, scratch, limits),
            limits,
        })
    }

    #[must_use]
    pub fn limits(&self) -> RenderResourceLimits {
        self.limits
    }

    /// Run the only command production rendering may execute. Missing OS
    /// confinement is an error rather than a fallback to a normal process.
    pub fn run<I, S>(
        &self,
        args: I,
        environment: &[(OsString, OsString)],
        options: BoundedProcessOptions,
    ) -> Result<BoundedProcessOutput, ConfinementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let deadline = Instant::now()
            .checked_add(options.timeout.min(self.limits.wall_timeout))
            .ok_or(BoundedProcessError::InvalidLifetime)?;
        self.run_checked_until(args, environment, options, deadline, || Ok(()))
    }

    /// Use one deadline through launcher preparation and execution. The
    /// supplied check runs at the platform's final renderer launch gate.
    pub fn run_checked_until<I, S>(
        &self,
        args: I,
        environment: &[(OsString, OsString)],
        options: BoundedProcessOptions,
        deadline: Instant,
        verify: impl Fn() -> std::io::Result<()>,
    ) -> Result<BoundedProcessOutput, ConfinementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let options = BoundedProcessOptions {
            timeout: options.timeout.min(self.limits.wall_timeout),
            ..options
        };
        let cap = Instant::now()
            .checked_add(options.timeout)
            .ok_or(BoundedProcessError::InvalidLifetime)?;
        let start = RendererStart {
            deadline: deadline.min(cap),
            budget: options.timeout,
            verify: &verify,
        };
        start.remaining()?;
        self.native.run(args, environment, options, start)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RendererStart<'a> {
    pub(crate) deadline: Instant,
    budget: Duration,
    verify: &'a dyn Fn() -> std::io::Result<()>,
}

impl RendererStart<'_> {
    pub(crate) fn remaining(self) -> Result<Duration, BoundedProcessError> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(BoundedProcessError::Timeout {
                timeout_ms: self.budget.as_millis(),
            })
        } else {
            Ok(remaining)
        }
    }

    pub(crate) fn verify(self) -> Result<(), BoundedProcessError> {
        self.remaining()?;
        (self.verify)().map_err(BoundedProcessError::Isolation)?;
        self.remaining()?;
        Ok(())
    }
}

/// The resolved executable and the exact runtime mappings used at launch.
/// Each mapping stores the requested path followed by its resolved source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSources {
    pub(crate) program: PathBuf,
    pub(crate) roots: Vec<(PathBuf, PathBuf)>,
}

impl RenderSources {
    pub fn resolve(
        program: &Path,
        runtime_roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, ConfinementError> {
        let program =
            canonical_file(program).ok_or_else(|| ConfinementError::Program(program.into()))?;
        let mut roots = Vec::new();
        for root in runtime_roots {
            if !is_normal_absolute_path(&root) {
                return Err(ConfinementError::Runtime(root));
            }
            let source =
                fs::canonicalize(&root).map_err(|_| ConfinementError::Runtime(root.clone()))?;
            if !source.is_absolute() {
                return Err(ConfinementError::Runtime(source));
            }
            roots.push((root, source));
        }
        Ok(Self { program, roots })
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    #[must_use]
    pub fn roots(&self) -> &[(PathBuf, PathBuf)] {
        &self.roots
    }
}

fn is_normal_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .as_os_str()
            .as_encoded_bytes()
            .split(|byte| *byte == b'/')
            .any(|component| matches!(component, b"." | b".."))
}

fn canonical_file(path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    path.is_absolute()
        .then_some(path)
        .filter(|path| path.is_file())
}

fn canonical_directory(path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    path.is_absolute()
        .then_some(path)
        .filter(|path| path.is_dir())
}

/// Inject an unavailable Linux isolation component at the diagnostic boundary.
/// Debug builds only; this always rejects and cannot authorize a renderer.
#[cfg(all(debug_assertions, target_os = "linux"))]
pub fn injected_linux_isolation_failure_for_test(selector: &str) -> ConfinementError {
    crate::platform::injected_isolation_failure(selector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_renderer_budget_stops_before_launch_verification() {
        let start = RendererStart {
            deadline: Instant::now(),
            budget: Duration::from_secs(10),
            verify: &|| panic!("launch verification started after deadline"),
        };
        assert!(matches!(
            start.verify(),
            Err(BoundedProcessError::Timeout { .. })
        ));
    }

    #[test]
    fn invalid_runtime_stops_at_launch_verification() {
        let start = RendererStart {
            deadline: Instant::now() + Duration::from_secs(10),
            budget: Duration::from_secs(10),
            verify: &|| Err(std::io::Error::other("runtime changed")),
        };
        let error = start.verify().unwrap_err();
        assert!(
            matches!(error, BoundedProcessError::Isolation(ref error) if error.to_string() == "runtime changed")
        );
    }
    #[test]
    fn expired_launch_budget_does_not_mask_isolation_failure() {
        let deadline = Instant::now() + Duration::from_millis(50);
        let start = RendererStart {
            deadline,
            budget: Duration::from_millis(50),
            verify: &|| {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
                );
                Err(std::io::Error::other("unverified launch isolation"))
            },
        };
        assert!(matches!(
            start.verify(),
            Err(BoundedProcessError::Isolation(_))
        ));
    }
}
