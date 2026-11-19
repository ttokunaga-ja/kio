//! Native Linux renderer state and bubblewrap command.
use crate::confinement::{ConfinementError, RenderResourceLimits, RenderSources, RendererStart};
use crate::{BoundedProcessOptions, BoundedProcessOutput};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Command, time::Duration};

#[derive(Debug, Clone)]
pub(crate) struct NativeRenderSandbox {
    pub(super) program: PathBuf,
    pub(super) scratch: PathBuf,
    pub(super) runtime_mounts: Vec<(PathBuf, PathBuf)>,
    masked_runtime_directory: Option<PathBuf>,
    pub(super) limits: RenderResourceLimits,
    #[cfg(test)]
    pub(super) test_fixture: super::confinement_cgroup::ConfinementFixture,
}

impl NativeRenderSandbox {
    pub(crate) fn new(
        sources: &RenderSources,
        scratch: PathBuf,
        limits: RenderResourceLimits,
    ) -> Self {
        let mut mounts = BTreeMap::new();
        for (destination, source) in &sources.roots {
            mounts.insert(destination.clone(), source.clone());
        }
        // Selecting an executable authorizes that file, not its siblings.
        // A standalone renderer may live next to private source documents or
        // credentials; package directories must be supplied explicitly.
        mounts
            .entry(sources.program.clone())
            .or_insert_with(|| sources.program.clone());
        Self {
            program: sources.program.clone(),
            scratch,
            runtime_mounts: mounts
                .into_iter()
                .map(|(destination, source)| (source, destination))
                .collect(),
            masked_runtime_directory: sources.masked_runtime_directory.clone(),
            limits,
            #[cfg(test)]
            test_fixture: Default::default(),
        }
    }
    pub(crate) fn run<I, S>(
        &self,
        args: I,
        environment: &[(OsString, OsString)],
        options: BoundedProcessOptions,
        start: RendererStart<'_>,
    ) -> Result<BoundedProcessOutput, ConfinementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        super::confinement_cgroup::run(self, args, environment, options, start)
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
        if let Some(directory) = &self.masked_runtime_directory {
            command
                .arg("--tmpfs")
                .arg(directory)
                .arg("--remount-ro")
                .arg(directory);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn runtime_mask_covers_a_leaf_of_the_read_only_mount() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let runtime = root.join("runtime");
        let cache = runtime.join("generated");
        fs::create_dir_all(&cache).unwrap();
        let program = std::env::current_exe().unwrap();
        let sources = RenderSources::resolve(&program, [runtime.clone()]).unwrap();
        assert!(
            sources
                .clone()
                .with_masked_runtime_directory(&root)
                .is_err()
        );

        let sources = sources.with_masked_runtime_directory(&cache).unwrap();
        let sandbox = NativeRenderSandbox::new(&sources, root, default_render_resource_limits());
        let command = sandbox.linux_command([] as [&str; 0]).unwrap();
        let args: Vec<_> = command.get_args().collect();
        let mount = args
            .windows(3)
            .position(|part| {
                part == [
                    "--ro-bind".as_ref(),
                    runtime.as_os_str(),
                    runtime.as_os_str(),
                ]
            })
            .unwrap();
        let mask = args
            .windows(4)
            .position(|part| {
                part == [
                    "--tmpfs".as_ref(),
                    cache.as_os_str(),
                    "--remount-ro".as_ref(),
                    cache.as_os_str(),
                ]
            })
            .unwrap();
        assert!(mount < mask);
    }
}
