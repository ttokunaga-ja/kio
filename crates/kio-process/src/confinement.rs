//! OS confinement for document renderers.
//!
//! A bounded process group alone cannot stop a compromised renderer from
//! reading the invoking user's files or using the network.  This module
//! configures the renderer's launch and resource limits. The default policy
//! requires OS access isolation. Windows Office explicitly selects Job limits.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::BoundedProcessError;
use crate::platform::{
    NativeRenderSandbox, RENDER_ADDRESS_SPACE_BYTES, RENDER_PHYSICAL_MEMORY_BYTES,
};
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
        Self {
            wall_timeout: Duration::from_secs(300),
            cpu_seconds: 300,
            max_aggregate_memory_bytes: 2 * 1024 * 1024 * 1024,
            max_address_space_bytes: RENDER_ADDRESS_SPACE_BYTES,
            max_physical_memory_bytes: RENDER_PHYSICAL_MEMORY_BYTES,
            max_file_bytes: 250 * 1024 * 1024,
        }
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

/// Immutable renderer launch policy with canonical paths and resource limits.
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

    /// Run with the selected renderer policy. Failure to establish it is an error.
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
            Err(self.timeout_error())
        } else {
            Ok(remaining)
        }
    }

    fn timeout_error(self) -> BoundedProcessError {
        BoundedProcessError::Timeout {
            timeout_ms: self.budget.as_millis(),
        }
    }

    pub(crate) fn verify(self) -> Result<(), BoundedProcessError> {
        self.remaining()?;
        (self.verify)().map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                self.timeout_error()
            } else {
                BoundedProcessError::Isolation(error)
            }
        })?;
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
    masked_runtime_directory: Option<PathBuf>,
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
        Self::checked(program, roots, None)
    }

    // An exclusion applies to every destination backed by the directory. Validate
    // the source and destination projections against the same declared mappings.
    pub(crate) fn checked(
        program: PathBuf,
        roots: Vec<(PathBuf, PathBuf)>,
        exclusion: Option<&Path>,
    ) -> Result<Self, ConfinementError> {
        let sources = Self {
            program,
            roots,
            masked_runtime_directory: exclusion.map(Path::to_path_buf),
        };
        if let Some(path) = exclusion {
            if !is_normal_absolute_path(path)
                || canonical_directory(path).as_deref() != Some(path)
                || sources.program.starts_with(path)
                || sources
                    .roots
                    .iter()
                    .any(|(_, source)| source != path && source.starts_with(path))
            {
                return Err(ConfinementError::Runtime(path.into()));
            }
            let destinations = sources.runtime_mask_destinations();
            if destinations.is_empty()
                || destinations.iter().any(|mask| {
                    sources.program.starts_with(mask)
                        || sources.roots.iter().any(|(destination, source)| {
                            destination.starts_with(mask) && (destination != mask || source != path)
                        })
                })
            {
                return Err(ConfinementError::Runtime(path.into()));
            }
        }
        Ok(sources)
    }

    pub(crate) fn runtime_mask_destinations(&self) -> Vec<PathBuf> {
        let Some(path) = self.masked_runtime_directory() else {
            return Vec::new();
        };
        let mut destinations: Vec<_> = self
            .roots
            .iter()
            .filter_map(|(destination, source)| {
                let suffix = path.strip_prefix(source).ok()?;
                Some(project_path(destination, suffix))
            })
            .collect();
        destinations.sort_unstable();
        destinations.dedup();
        destinations
    }

    #[must_use]
    pub fn masked_runtime_directory(&self) -> Option<&Path> {
        self.masked_runtime_directory.as_deref()
    }

    /// Resolve a sandbox path against the declared file and directory mappings.
    /// For nested mappings the most specific destination takes precedence.
    pub fn sandbox_path_source(&self, path: &Path) -> Option<PathBuf> {
        if path == self.program {
            return Some(path.to_path_buf());
        }
        self.roots
            .iter()
            .filter_map(|(destination, source)| {
                let suffix = path.strip_prefix(destination).ok()?;
                Some((destination, source, suffix))
            })
            .max_by_key(|(destination, _, _)| destination.as_os_str().len())
            .map(|(_, source, suffix)| project_path(source, suffix))
    }

    /// Whether a sandbox path is at or above one of the declared destinations.
    pub fn contains_sandbox_destination(&self, path: &Path) -> bool {
        self.program.starts_with(path)
            || self
                .roots
                .iter()
                .any(|(destination, _)| destination.starts_with(path))
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

fn project_path(root: &Path, suffix: &Path) -> PathBuf {
    if suffix.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(suffix)
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
    fn launch_verification_preserves_a_typed_deadline_failure() {
        let start = RendererStart {
            deadline: Instant::now() + Duration::from_secs(10),
            budget: Duration::from_secs(10),
            verify: &|| Err(std::io::ErrorKind::TimedOut.into()),
        };
        assert!(matches!(
            start.verify(),
            Err(BoundedProcessError::Timeout { timeout_ms: 10_000 })
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
