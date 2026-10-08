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
    time::Duration,
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
        Ok(Self {
            native: NativeRenderSandbox::new(program, scratch, runtime_roots, limits)?,
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
        let options = BoundedProcessOptions {
            timeout: options.timeout.min(self.limits.wall_timeout),
            ..options
        };
        self.native.run(args, environment, options)
    }
}

pub(crate) fn canonical_paths(
    program: &Path,
    scratch: &Path,
    runtime_roots: impl IntoIterator<Item = PathBuf>,
) -> Result<CanonicalRenderPaths, ConfinementError> {
    let program =
        canonical_file(program).ok_or_else(|| ConfinementError::Program(program.into()))?;
    let scratch =
        canonical_directory(scratch).ok_or_else(|| ConfinementError::Scratch(scratch.into()))?;
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
    Ok(CanonicalRenderPaths {
        program,
        scratch,
        roots,
    })
}

pub(crate) struct CanonicalRenderPaths {
    pub(crate) program: PathBuf,
    pub(crate) scratch: PathBuf,
    pub(crate) roots: Vec<(PathBuf, PathBuf)>,
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
