//! Native macOS renderer boundary and Seatbelt profile.
use crate::confinement::{ConfinementError, RenderResourceLimits, RenderSources, RendererStart};
use crate::platform::{apply_unix_limits, run_bounded_command_with_unix_renderer_limits};
use crate::{BoundedProcessOptions, BoundedProcessOutput};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub(crate) struct NativeRenderSandbox {
    program: PathBuf,
    scratch: PathBuf,
    runtime_roots: Vec<PathBuf>,
    limits: RenderResourceLimits,
}

impl NativeRenderSandbox {
    pub(crate) fn new(
        sources: &RenderSources,
        scratch: PathBuf,
        limits: RenderResourceLimits,
    ) -> Self {
        let mut roots: BTreeSet<_> = sources
            .roots
            .iter()
            .map(|(_, source)| source.clone())
            .collect();
        // Selecting an executable authorizes that file, not its siblings.
        // A standalone renderer may live next to private source documents or
        // credentials; package directories must be supplied explicitly.
        roots.insert(sources.program.clone());
        Self {
            program: sources.program.clone(),
            scratch,
            runtime_roots: roots.into_iter().collect(),
            limits,
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
        let mut command = self.macos_command(args)?;
        command.env_clear().envs(environment.iter().cloned());
        Ok(run_bounded_command_with_unix_renderer_limits(
            &mut command,
            options,
            None,
            &self.scratch,
            self.limits.max_file_bytes,
            self.limits.max_physical_memory_bytes,
            start,
        )?)
    }

    fn macos_command<I, S>(&self, args: I) -> Result<Command, ConfinementError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let sandbox_exec = Path::new("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            return Err(ConfinementError::BackendUnavailable(
                "/usr/bin/sandbox-exec",
            ));
        }
        let profile = self.scratch.join(".kio-render.sb");
        let profile_source = macos_profile(&self.scratch, &self.runtime_roots);
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut profile_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&profile)
            .map_err(ConfinementError::Profile)?;
        profile_file
            .write_all(profile_source.as_bytes())
            .map_err(ConfinementError::Profile)?;
        let mut command = Command::new(sandbox_exec);
        command
            .arg("-f")
            .arg(profile)
            .arg(&self.program)
            .args(args)
            .current_dir(&self.scratch);
        apply_unix_limits(&mut command, self.limits, true)?;
        Ok(command)
    }
}

pub(crate) const RENDER_ADDRESS_SPACE_BYTES: u64 = 0;
pub(crate) const RENDER_PHYSICAL_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn macos_profile(scratch: &Path, runtime_roots: &[PathBuf]) -> String {
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (import \"system.sb\")\n\
         (deny network*)\n\
         (deny process-fork)\n\
         (allow process-info* signal (target self))\n",
    );
    // Dynamic loading and fonts require read access only to the explicitly
    // enumerated runtime roots. No home directory or filesystem root is added.
    for root in runtime_roots {
        // LibreOffice resolves its bootstrap registry through realpath(),
        // which stats every ancestor even when the final file is readable.
        // Permit only traversal metadata, never ancestor directory contents.
        profile.push_str(&format!(
            "(allow file-read-metadata file-test-existence (path-ancestors {}))\n",
            sandbox_literal(root)
        ));
        profile.push_str(&format!(
            "(allow file-read* file-test-existence file-map-executable process-exec (subpath {}))\n",
            sandbox_literal(root)
        ));
    }
    profile.push_str(&format!(
        "(allow file-read-metadata file-test-existence (path-ancestors {}))\n",
        sandbox_literal(scratch)
    ));
    profile.push_str(&format!(
        "(allow file-read* file-test-existence file-write* (subpath {}))\n",
        sandbox_literal(scratch)
    ));
    // LibreOffice's single-instance IPC uses a filesystem Unix socket. Only
    // this invocation's private scratch may host or receive that traffic;
    // these pathname filters grant no IP network or host socket access.
    profile.push_str(&format!(
        "(allow network-bind network-inbound network-outbound (subpath {}))\n",
        sandbox_literal(scratch)
    ));
    profile.push_str("(allow file-read* (literal \"/dev/null\"))\n");
    profile.push_str("(allow file-read* (literal \"/dev/urandom\"))\n");
    profile
}

fn sandbox_literal(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
