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

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(not(target_os = "linux"))]
use std::collections::BTreeSet;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

use thiserror::Error;

#[cfg(target_os = "linux")]
#[path = "confinement_linux_cgroup.rs"]
mod linux_cgroup;
#[cfg(target_os = "linux")]
#[path = "confinement_linux_mounts.rs"]
mod linux_mounts;

#[cfg(target_os = "macos")]
use crate::run_bounded_command_with_unix_renderer_limits;
use crate::{BoundedProcessError, BoundedProcessOptions, BoundedProcessOutput};

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
            // Darwin's shared-cache reservation makes RLIMIT_AS unsuitable
            // as a resident-memory budget. Until a process-tree footprint
            // monitor exists, macOS disables this kernel limit explicitly.
            #[cfg(target_os = "macos")]
            max_address_space_bytes: 0,
            #[cfg(not(target_os = "macos"))]
            max_address_space_bytes: 2 * 1024 * 1024 * 1024,
            // Darwin reserves the shared cache in virtual address space, so
            // use its documented physical-footprint accounting instead. The
            // monitor intentionally observes only the direct renderer: the
            // macOS seatbelt profile denies process-fork.
            #[cfg(target_os = "macos")]
            max_physical_memory_bytes: 2 * 1024 * 1024 * 1024,
            #[cfg(not(target_os = "macos"))]
            max_physical_memory_bytes: 0,
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

/// Establish the protected owner-only DACL required for a newly allocated
/// renderer scratch directory.  Callers must do this before placing any input
/// or renderer profile data in the directory; the Windows renderer then adds
/// only its fresh AppContainer SID for that invocation.
#[cfg(windows)]
pub fn protect_owner_private_scratch(path: &Path) -> Result<(), ConfinementError> {
    crate::confinement_windows::protect_owner_private_scratch(path)
}

/// Immutable renderer boundary.  All path values are canonicalized before the
/// child is created, and no ambient home or filesystem root is admitted.
#[derive(Debug, Clone)]
pub struct RenderSandbox {
    program: PathBuf,
    scratch: PathBuf,
    #[cfg(not(target_os = "linux"))]
    runtime_roots: Vec<PathBuf>,
    #[cfg(target_os = "linux")]
    runtime_mounts: Vec<(PathBuf, PathBuf)>,
    limits: RenderResourceLimits,
}

impl RenderSandbox {
    pub fn new(
        program: &Path,
        scratch: &Path,
        runtime_roots: impl IntoIterator<Item = PathBuf>,
        limits: RenderResourceLimits,
    ) -> Result<Self, ConfinementError> {
        let program =
            canonical_file(program).ok_or_else(|| ConfinementError::Program(program.into()))?;
        let scratch = canonical_directory(scratch)
            .ok_or_else(|| ConfinementError::Scratch(scratch.into()))?;
        #[cfg(not(target_os = "linux"))]
        let mut roots = BTreeSet::new();
        #[cfg(target_os = "linux")]
        let mut mounts = BTreeMap::new();
        for root in runtime_roots {
            if !is_normal_absolute_path(&root) {
                return Err(ConfinementError::Runtime(root));
            }
            let source =
                fs::canonicalize(&root).map_err(|_| ConfinementError::Runtime(root.clone()))?;
            if !source.is_absolute() {
                return Err(ConfinementError::Runtime(source));
            }
            #[cfg(not(target_os = "linux"))]
            roots.insert(source.clone());
            #[cfg(target_os = "linux")]
            mounts.insert(root, source);
        }
        // Selecting an executable authorizes that file, not its siblings.
        // A standalone renderer may live next to private source documents or
        // credentials; package directories must be supplied explicitly.
        #[cfg(not(target_os = "linux"))]
        roots.insert(program.clone());
        #[cfg(target_os = "linux")]
        mounts
            .entry(program.clone())
            .or_insert_with(|| program.clone());
        Ok(Self {
            program,
            scratch,
            #[cfg(not(target_os = "linux"))]
            runtime_roots: roots.into_iter().collect(),
            #[cfg(target_os = "linux")]
            runtime_mounts: mounts
                .into_iter()
                .map(|(destination, source)| (source, destination))
                .collect(),
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
        #[cfg(target_os = "macos")]
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
            )?)
        }
        #[cfg(target_os = "linux")]
        {
            linux_cgroup::run(self, args, environment, options)
        }
        #[cfg(windows)]
        {
            crate::confinement_windows::run_windows_renderer(self, args, environment, options)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            let _ = (args, environment, options);
            Err(ConfinementError::BackendUnavailable(
                "unsupported operating system",
            ))
        }
    }

    #[cfg(target_os = "macos")]
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
        let mut profile_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
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
        apply_unix_limits(&mut command, self.limits)?;
        Ok(command)
    }

    #[cfg(target_os = "linux")]
    fn linux_command<I, S>(&self, args: I) -> Result<Command, ConfinementError>
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

    #[cfg(windows)]
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }
    #[cfg(windows)]
    pub(crate) fn scratch(&self) -> &Path {
        &self.scratch
    }
    #[cfg(windows)]
    pub(crate) fn runtime_roots(&self) -> &[PathBuf] {
        &self.runtime_roots
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

#[cfg(unix)]
fn apply_unix_limits(
    command: &mut Command,
    limits: RenderResourceLimits,
) -> Result<(), ConfinementError> {
    use std::os::unix::process::CommandExt;
    let cpu = libc::rlim_t::try_from(limits.cpu_seconds).map_err(|_| {
        ConfinementError::Resource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CPU limit out of range",
        ))
    })?;
    let address_space = libc::rlim_t::try_from(limits.max_address_space_bytes).map_err(|_| {
        ConfinementError::Resource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "address-space limit out of range",
        ))
    })?;
    let file_size = libc::rlim_t::try_from(limits.max_file_bytes).map_err(|_| {
        ConfinementError::Resource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file-size limit out of range",
        ))
    })?;
    unsafe {
        command.pre_exec(move || {
            for (resource, value, optional) in [
                (libc::RLIMIT_CPU, cpu, false),
                // Darwin exposes RLIMIT_AS but does not consistently support
                // applying it to every process class. CPU and output-file
                // limits remain mandatory; unsupported address-space limits
                // are handled by the platform-specific Windows Job path.
                (libc::RLIMIT_AS, address_space, cfg!(target_os = "macos")),
                (libc::RLIMIT_FSIZE, file_size, false),
            ] {
                if optional && value == 0 {
                    continue;
                }
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    if optional
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                    {
                        continue;
                    }
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn sandbox_literal(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
