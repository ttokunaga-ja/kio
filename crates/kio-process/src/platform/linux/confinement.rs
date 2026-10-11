//! Native Linux renderer state and bubblewrap command.
use crate::confinement::{ConfinementError, RenderResourceLimits, canonical_paths};
use crate::{BoundedProcessOptions, BoundedProcessOutput};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Debug, Clone)]
pub(crate) struct NativeRenderSandbox {
    pub(super) program: PathBuf,
    pub(super) scratch: PathBuf,
    pub(super) runtime_mounts: Vec<(PathBuf, PathBuf)>,
    pub(super) limits: RenderResourceLimits,
    #[cfg(test)]
    pub(super) test_fixture: super::confinement_cgroup::ConfinementFixture,
}

impl NativeRenderSandbox {
    pub(crate) fn new(
        program: &Path,
        scratch: &Path,
        runtime_roots: impl IntoIterator<Item = PathBuf>,
        limits: RenderResourceLimits,
    ) -> Result<Self, ConfinementError> {
        let paths = canonical_paths(program, scratch, runtime_roots)?;
        let mut mounts = BTreeMap::new();
        for (destination, source) in paths.roots {
            mounts.insert(destination, source);
        }
        // Selecting an executable authorizes that file, not its siblings.
        // A standalone renderer may live next to private source documents or
        // credentials; package directories must be supplied explicitly.
        mounts
            .entry(paths.program.clone())
            .or_insert_with(|| paths.program.clone());
        Ok(Self {
            program: paths.program,
            scratch: paths.scratch,
            runtime_mounts: mounts
                .into_iter()
                .map(|(destination, source)| (source, destination))
                .collect(),
            limits,
            #[cfg(test)]
            test_fixture: Default::default(),
        })
    }
    pub(crate) fn run<I, S>(
        &self,
        args: I,
        environment: &[(OsString, OsString)],
        options: BoundedProcessOptions,
    ) -> Result<BoundedProcessOutput, ConfinementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        super::confinement_cgroup::run(self, args, environment, options)
    }

    pub(super) fn linux_command<I, S>(&self, args: I) -> Result<Command, ConfinementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new("/usr/bin/bwrap");
        command
            .arg("--unshare-net")
            // A procfs mount without a private PID namespace exposes host
            // processes, including same-user environments and proc/root links.
            .arg("--unshare-pid")
            .arg("--die-with-parent")
            .arg("--new-session")
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev")
            .arg("--tmpfs")
            .arg("/tmp");
        for (source, destination) in &self.runtime_mounts {
            command.arg("--ro-bind").arg(source).arg(destination);
        }
        command
            .arg("--bind")
            .arg(&self.scratch)
            .arg(&self.scratch)
            .arg("--chdir")
            .arg(&self.scratch)
            .arg("--")
            .arg(&self.program)
            .args(args);
        Ok(command)
    }
}

pub(crate) fn default_render_resource_limits() -> RenderResourceLimits {
    RenderResourceLimits {
        wall_timeout: Duration::from_secs(300),
        cpu_seconds: 300,
        max_aggregate_memory_bytes: 2 * 1024 * 1024 * 1024,
        max_address_space_bytes: 2 * 1024 * 1024 * 1024,
        max_physical_memory_bytes: 0,
        max_file_bytes: 250 * 1024 * 1024,
    }
}
