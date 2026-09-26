//! Small, descriptor-only process capabilities shared by evaluator fixtures.
//!
//! This is intentionally not a process runner.  Callers retain their own
//! freshness and output policies; this module only makes executable, cwd, and
//! HOME/XDG authority survive pathname replacement on Linux.

use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Command,
};

#[cfg(target_os = "linux")]
use std::{ffi::CString, io::Write};

use cap_primitives::{ambient_authority, fs as cap_fs};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) type ProcessBoundaryResult<T> = Result<T, ProcessBoundaryError>;

#[derive(Debug, Error)]
pub(crate) enum ProcessBoundaryError {
    #[error("unsafe descriptor process boundary at {path}: {message}")]
    Unsafe { path: PathBuf, message: String },
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[error("descriptor process boundary is unsupported on this platform: {0}")]
    Unsupported(&'static str),
}

impl ProcessBoundaryError {
    fn unsafe_(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Unsafe {
            path: path.into(),
            message: message.into(),
        }
    }
    fn io(path: impl Into<PathBuf>, error: io::Error) -> Self {
        Self::unsafe_(path, error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: u64,
}

/// A supplied absolute executable whose original inode and sealed execution
/// copy are both retained.  `command` never executes a pathname supplied by a
/// caller.
#[derive(Debug)]
pub(crate) struct DescriptorExecutable {
    diagnostic_path: PathBuf,
    parent: fs::File,
    name: String,
    link_policy: LinkPolicy,
    observation: SourceObservation,
    immutable_binding: ExecutableBinding,
    #[cfg(target_os = "linux")]
    sealed: fs::File,
}

impl DescriptorExecutable {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn preflight_platform() -> ProcessBoundaryResult<()> {
        #[cfg(target_os = "linux")]
        {
            if fs::metadata("/dev/fd")
                .map_err(|e| ProcessBoundaryError::io("/dev/fd", e))?
                .is_dir()
            {
                Ok(())
            } else {
                Err(ProcessBoundaryError::Unsupported(
                    "Linux descriptor execution requires /dev/fd",
                ))
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ProcessBoundaryError::Unsupported(
                "descriptor-bound executable execution",
            ))
        }
    }

    pub(crate) fn bind(source: &Path) -> ProcessBoundaryResult<Self> {
        Self::bind_with_link_policy(source, LinkPolicy::ExactlyOne)
    }

    /// Bind a release-build artifact used only by the scale fixture commands.
    ///
    /// Cargo may retain a second hard link from `target/release/deps/` to the
    /// canonical `target/release/kio` binary.  Unlike `bind`, this permits the
    /// one-link or Cargo two-link build-artifact shapes, but retains and rechecks the exact link
    /// count together with the inode, length, mode, and digest before every
    /// execution.  Do not use this for replay inputs.
    pub(crate) fn bind_build_artifact(source: &Path) -> ProcessBoundaryResult<Self> {
        Self::bind_with_link_policy(source, LinkPolicy::OneOrTwo)
    }

    fn bind_with_link_policy(
        source: &Path,
        link_policy: LinkPolicy,
    ) -> ProcessBoundaryResult<Self> {
        if !source.is_absolute() {
            return Err(ProcessBoundaryError::unsafe_(
                source,
                "binary path must be absolute",
            ));
        }
        let (parent_path, leaf) = source.parent().zip(source.file_name()).ok_or_else(|| {
            ProcessBoundaryError::unsafe_(source, "binary has no parent or filename")
        })?;
        let leaf = leaf
            .to_str()
            .ok_or_else(|| ProcessBoundaryError::unsafe_(source, "binary filename is not UTF-8"))?;
        single_component(leaf, "binary filename")?;
        let parent = open_lexical_directory(parent_path)?;
        let observed =
            observe_executable(&parent, leaf, source, MAX_EXECUTABLE_BYTES, link_policy)?;
        #[cfg(target_os = "linux")]
        let sealed = sealed_memfd(&observed.bytes)?;
        let result = Self {
            diagnostic_path: source.to_owned(),
            parent,
            name: leaf.to_owned(),
            link_policy,
            observation: observed.observation(),
            immutable_binding: observed.immutable_binding(),
            #[cfg(target_os = "linux")]
            sealed,
        };
        result.recheck_original()?;
        Ok(result)
    }
    /// The immutable source digest and length captured for the sealed bytes.
    /// This accessor never reopens the supplied pathname.
    #[must_use]
    pub(crate) fn immutable_binding(&self) -> &ExecutableBinding {
        &self.immutable_binding
    }
    pub(crate) fn recheck_original(&self) -> ProcessBoundaryResult<()> {
        let observed = observe_executable(
            &self.parent,
            &self.name,
            &self.diagnostic_path,
            MAX_EXECUTABLE_BYTES,
            self.link_policy,
        )?;
        if observed.observation() != self.observation
            || &observed.immutable_binding() != self.immutable_binding()
        {
            return Err(ProcessBoundaryError::unsafe_(
                &self.diagnostic_path,
                "original executable identity changed",
            ));
        }
        #[cfg(target_os = "linux")]
        verify_sealed_memfd(&self.sealed)?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn command(&self) -> ProcessBoundaryResult<Command> {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        self.recheck_original()?;
        let retained = self
            .sealed
            .try_clone()
            .map_err(|e| ProcessBoundaryError::io(&self.diagnostic_path, e))?;
        let fd = retained.as_raw_fd();
        let mut command = Command::new(format!("/dev/fd/{fd}"));
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(retained.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        Ok(command)
    }
    #[cfg(not(target_os = "linux"))]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn command(&self) -> ProcessBoundaryResult<Command> {
        Err(ProcessBoundaryError::Unsupported(
            "descriptor-bound executable command",
        ))
    }
    #[cfg(target_os = "linux")]
    #[cfg(test)]
    pub(crate) fn sealed_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.sealed.as_raw_fd()
    }
}

/// Configure a command with only deterministic variables and descriptor-backed
/// private roots. The supplied handles must outlive command construction only;
/// clones are retained through exec.
#[cfg(target_os = "linux")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn configure_descriptor_environment(
    command: &mut Command,
    directories: &[(&str, &fs::File)],
) -> ProcessBoundaryResult<()> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    DescriptorExecutable::preflight_platform()?;
    command.env_clear();
    command.env("PATH", "/usr/bin:/bin");
    command.env("LANG", "C.UTF-8");
    command.env("LC_ALL", "C.UTF-8");
    command.env("TZ", "UTC");
    for (variable, directory) in directories {
        let retained = directory
            .try_clone()
            .map_err(|e| ProcessBoundaryError::io(format!("descriptor:{variable}"), e))?;
        let fd = retained.as_raw_fd();
        command.env(variable, format!("/dev/fd/{fd}"));
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(retained.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn configure_descriptor_environment(
    _command: &mut Command,
    _directories: &[(&str, &fs::File)],
) -> ProcessBoundaryResult<()> {
    Err(ProcessBoundaryError::Unsupported(
        "descriptor-backed hermetic environment",
    ))
}

#[cfg(unix)]
pub(crate) fn configure_retained_cwd(
    command: &mut Command,
    directory: &fs::File,
) -> ProcessBoundaryResult<()> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let retained = directory
        .try_clone()
        .map_err(|e| ProcessBoundaryError::io("retained cwd", e))?;
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(retained.as_raw_fd()) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn configure_retained_cwd(
    _command: &mut Command,
    _directory: &fs::File,
) -> ProcessBoundaryResult<()> {
    Err(ProcessBoundaryError::Unsupported(
        "retained-handle subprocess cwd",
    ))
}

fn single_component(value: &str, label: &str) -> ProcessBoundaryResult<()> {
    let mut components = Path::new(value).components();
    if value.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        Err(ProcessBoundaryError::unsafe_(
            value,
            format!("invalid {label}"),
        ))
    } else {
        Ok(())
    }
}

/// Open each absolute ancestor through a retained descriptor.  This avoids
/// accepting an executable hidden behind a user-controlled parent symlink.
#[cfg(unix)]
fn open_lexical_directory(path: &Path) -> ProcessBoundaryResult<fs::File> {
    let path = normalize_os_alias(path)?;
    if !path.is_absolute() {
        return Err(ProcessBoundaryError::unsafe_(
            &path,
            "binary parent must be absolute",
        ));
    }
    let mut handle = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())
        .map_err(|e| ProcessBoundaryError::io("/", e))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                handle = cap_fs::open_dir_nofollow(&handle, Path::new(name)).map_err(|_| {
                    ProcessBoundaryError::unsafe_(
                        &path,
                        "binary parent ancestor must be a real directory",
                    )
                })?;
            }
            _ => {
                return Err(ProcessBoundaryError::unsafe_(
                    &path,
                    "binary parent is not lexical absolute components",
                ));
            }
        }
    }
    Ok(handle)
}
#[cfg(not(unix))]
fn open_lexical_directory(path: &Path) -> ProcessBoundaryResult<fs::File> {
    cap_fs::open_ambient_dir(path, ambient_authority())
        .map_err(|e| ProcessBoundaryError::io(path, e))
}

#[cfg(target_os = "macos")]
fn normalize_os_alias(path: &Path) -> ProcessBoundaryResult<PathBuf> {
    let value = path
        .to_str()
        .ok_or_else(|| ProcessBoundaryError::unsafe_(path, "binary parent is not UTF-8"))?;
    let rewritten = if value == "/tmp"
        || value.starts_with("/tmp/")
        || value == "/var"
        || value.starts_with("/var/")
    {
        format!("/private{value}")
    } else {
        value.to_owned()
    };
    Ok(PathBuf::from(rewritten))
}
#[cfg(not(target_os = "macos"))]
fn normalize_os_alias(path: &Path) -> ProcessBoundaryResult<PathBuf> {
    Ok(path.to_owned())
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutableBinding {
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceObservation {
    identity: FileIdentity,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    links: u64,
}

/// The ordinary evaluator/replay contract binds a one-link input.  Only the
/// scale command accepts Cargo's canonical release artifact, which has either
/// one link or a single retained hard-link alias in `target/release/deps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkPolicy {
    ExactlyOne,
    OneOrTwo,
}

#[derive(Debug)]
struct ObservedExecutable {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    bytes: Vec<u8>,
    observation: SourceObservation,
    immutable_binding: ExecutableBinding,
}

impl ObservedExecutable {
    fn observation(&self) -> SourceObservation {
        self.observation.clone()
    }
    fn immutable_binding(&self) -> ExecutableBinding {
        self.immutable_binding.clone()
    }
}

fn observe_executable(
    parent: &fs::File,
    name: &str,
    diagnostic: &Path,
    max: u64,
    link_policy: LinkPolicy,
) -> ProcessBoundaryResult<ObservedExecutable> {
    let mut opts = cap_fs::OpenOptions::new();
    opts.read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(parent, Path::new(name), &opts)
        .map_err(|e| ProcessBoundaryError::io(diagnostic, e))?;
    let before =
        cap_fs::Metadata::from_file(&file).map_err(|e| ProcessBoundaryError::io(diagnostic, e))?;
    if !before.file_type().is_file() {
        return Err(ProcessBoundaryError::unsafe_(
            diagnostic,
            "executable must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        let valid_links = match link_policy {
            LinkPolicy::ExactlyOne => before.nlink() == 1,
            LinkPolicy::OneOrTwo => (1..=2).contains(&before.nlink()),
        };
        if !valid_links {
            return Err(ProcessBoundaryError::unsafe_(
                diagnostic,
                match link_policy {
                    LinkPolicy::ExactlyOne => "executable must have exactly one link",
                    LinkPolicy::OneOrTwo => {
                        "scale build artifact must have one link or Cargo's two-link shape"
                    }
                },
            ));
        }
    }
    if before.len() > max {
        return Err(ProcessBoundaryError::unsafe_(
            diagnostic,
            "executable exceeds byte bound",
        ));
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if before.mode() & 0o111 == 0 {
            return Err(ProcessBoundaryError::unsafe_(
                diagnostic,
                "binary is not executable",
            ));
        }
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|e| ProcessBoundaryError::io(diagnostic, e))?;
    let after =
        cap_fs::Metadata::from_file(&file).map_err(|e| ProcessBoundaryError::io(diagnostic, e))?;
    if !after.file_type().is_file()
        || file_identity(&after) != file_identity(&before)
        || after.len() != before.len()
        || bytes.len() as u64 != before.len()
        || metadata_changed_during_read(&before, &after)
    {
        return Err(ProcessBoundaryError::unsafe_(
            diagnostic,
            "executable changed while reading",
        ));
    }
    Ok(ObservedExecutable {
        observation: SourceObservation {
            identity: file_identity(&after),
            #[cfg(unix)]
            mode: executable_mode(&after),
            #[cfg(unix)]
            links: link_count(&after),
        },
        immutable_binding: ExecutableBinding {
            sha256: sha256(&bytes),
            bytes: bytes.len() as u64,
        },
        bytes,
    })
}
#[cfg(unix)]
fn executable_mode(metadata: &cap_fs::Metadata) -> u32 {
    use cap_fs::MetadataExt;
    metadata.mode()
}
#[cfg(unix)]
fn link_count(metadata: &cap_fs::Metadata) -> u64 {
    use cap_fs::MetadataExt;
    metadata.nlink()
}
#[cfg(unix)]
fn metadata_changed_during_read(before: &cap_fs::Metadata, after: &cap_fs::Metadata) -> bool {
    executable_mode(before) != executable_mode(after) || link_count(before) != link_count(after)
}
#[cfg(not(unix))]
fn metadata_changed_during_read(_before: &cap_fs::Metadata, _after: &cap_fs::Metadata) -> bool {
    false
}
fn file_identity(meta: &cap_fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        FileIdentity {
            volume: meta.volume_serial_number().unwrap_or(0),
            index: meta.file_index().unwrap_or(0),
        }
    }
}
fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(target_os = "linux")]
const MEMFD_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
#[cfg(target_os = "linux")]
fn sealed_memfd(bytes: &[u8]) -> ProcessBoundaryResult<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    const MFD_CLOEXEC: libc::c_uint = 0x0001;
    const MFD_ALLOW_SEALING: libc::c_uint = 0x0002;
    let name = CString::new("kio-eval-exec").expect("literal");
    let raw = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr(),
            MFD_CLOEXEC | MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(ProcessBoundaryError::io(
            "memfd:kio-eval-exec",
            io::Error::last_os_error(),
        ));
    }
    let mut file = unsafe { fs::File::from_raw_fd(raw as i32) };
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| ProcessBoundaryError::io("memfd:kio-eval-exec", e))?;
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } != 0 {
        return Err(ProcessBoundaryError::io(
            "memfd:kio-eval-exec",
            io::Error::last_os_error(),
        ));
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, MEMFD_SEALS) } == -1 {
        return Err(ProcessBoundaryError::io(
            "memfd:kio-eval-exec",
            io::Error::last_os_error(),
        ));
    }
    verify_sealed_memfd(&file)?;
    Ok(file)
}
#[cfg(target_os = "linux")]
fn verify_sealed_memfd(file: &fs::File) -> ProcessBoundaryResult<()> {
    use std::os::fd::AsRawFd;
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals == -1 || (seals & MEMFD_SEALS) != MEMFD_SEALS {
        Err(ProcessBoundaryError::unsafe_(
            "memfd:kio-eval-exec",
            "executable memfd seals are incomplete",
        ))
    } else {
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    #[test]
    fn env_is_allowlisted_and_descriptor_backed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let f = fs::File::open(&home).unwrap();
        let mut c = Command::new("/bin/sh");
        c.arg("-c")
            .arg("test -z \"${SECRET+x}\" && printf ok > \"$HOME/x\"");
        c.env("SECRET", "no");
        configure_descriptor_environment(&mut c, &[("HOME", &f)]).unwrap();
        assert!(c.status().unwrap().success());
        assert_eq!(fs::read(home.join("x")).unwrap(), b"ok");
    }
    #[test]
    fn descriptor_environment_child_opens_ledger_through_inherited_xdg_fd() {
        const CHILD_MARKER: &str = "KIO_EVAL_DESCRIPTOR_LEDGER_CHILD";
        const MONTH_MARKER: &str = "KIO_EVAL_DESCRIPTOR_LEDGER_MONTH";
        if matches!(std::env::var(CHILD_MARKER).as_deref(), Ok("1")) {
            use std::os::unix::ffi::OsStrExt;

            let xdg_data = std::env::var_os("XDG_DATA_HOME")
                .expect("child must receive descriptor-backed XDG_DATA_HOME");
            let raw = xdg_data.as_bytes();
            let descriptor = raw
                .strip_prefix(b"/dev/fd/")
                .expect("child XDG_DATA_HOME must use the exact /dev/fd/N spelling");
            assert!(
                !descriptor.is_empty()
                    && descriptor.iter().all(u8::is_ascii_digit)
                    && !(descriptor.len() > 1 && descriptor[0] == b'0'),
                "child XDG_DATA_HOME descriptor must be canonical"
            );
            let ledger = PathBuf::from(xdg_data).join("kio/cost-ledger.sqlite");
            let snapshot = kio_pipeline::ledger::LedgerReadSnapshot::open(&ledger)
                .expect("child must bind the inherited XDG descriptor");
            let month = std::env::var(MONTH_MARKER).expect("parent supplies the recorded month");
            assert_eq!(snapshot.month_total(None, None, &month).unwrap(), 2.0);
            return;
        }

        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let data = temp.path().join("data");
        fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
        let ledger_dir = data.join("kio");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&ledger_dir)
            .unwrap();
        let ledger = ledger_dir.join("cost-ledger.sqlite");
        use kio_pipeline::ledger::{BatchState, Outcome, RequestKind, TaskKey, ops};
        let db = kio_pipeline::ledger::LedgerDb::initialize(&ledger).unwrap();
        let key = TaskKey {
            scope_id: "descriptor-child".into(),
            adapter_kind: "embedding".into(),
            input_hash: "input".into(),
            tool_profile_hash: "profile".into(),
        };
        let intent = ops::phase1_intent(&db, &key, RequestKind::Sync, 2.0, Some(30)).unwrap();
        assert!(
            ops::sync_record_provider_request_id(&db, &key, &intent.intent_token, "job").unwrap()
        );
        let receipt = ops::terminal_transaction(
            &db,
            &ops::TerminalWrite {
                key: &key,
                outcome: Outcome::Succeeded,
                billed: ops::BilledAmount {
                    usd: 2.0,
                    estimated: false,
                },
                ledger_batch_job_id: "job",
                next_state: BatchState::Completed,
                error: None,
                increment_contract_violation: false,
                attempts_delta: 0,
                clear_intent_token: true,
                intent_token_guard: Some(&intent.intent_token),
                reseat_submission_seq: false,
            },
        )
        .unwrap();
        assert!(receipt.recorded);
        // The parent's durable row supplies the month, including a UTC month
        // boundary between the settlement and the child process starting.
        let rows = ops::cost_ledger_rows_for_key(&db, &key).unwrap();
        assert_eq!(rows.len(), 1);
        let month = rows[0].month.clone();
        drop(db);
        let data_handle = fs::File::open(&data).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .arg("--exact")
            .arg("process_boundary::tests::descriptor_environment_child_opens_ledger_through_inherited_xdg_fd")
            .arg("--nocapture");
        configure_descriptor_environment(&mut child, &[("XDG_DATA_HOME", &data_handle)]).unwrap();
        // This marker is deliberately set after `configure_descriptor_environment`,
        // which clears the inherited environment by design.
        child.env(CHILD_MARKER, "1");
        child.env(MONTH_MARKER, month);
        assert!(child.status().unwrap().success());
    }
    #[test]
    fn executable_rechecks_and_memfd_is_sealed() {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path().join("x");
        fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        let e = DescriptorExecutable::bind(&p).unwrap();
        assert_eq!(
            unsafe { libc::pwrite(e.sealed_fd(), b"X".as_ptr().cast(), 1, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        fs::rename(&p, temp.path().join("y")).unwrap();
        assert!(e.recheck_original().is_err());
    }
    #[test]
    fn retained_cwd() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("x"), b"yes").unwrap();
        let d = fs::File::open(temp.path()).unwrap();
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg("test -f x");
        configure_retained_cwd(&mut c, &d).unwrap();
        assert!(c.status().unwrap().success());
    }
}

#[cfg(all(test, unix))]
mod executable_binding_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(temp: &tempfile::TempDir) -> PathBuf {
        let path = temp.path().join("kio-under-test");
        fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn rejects_same_content_source_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(&temp);
        let bound = DescriptorExecutable::bind(&path).unwrap();
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(bound.recheck_original().is_err());
    }

    #[test]
    fn rejects_executable_mode_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(&temp);
        let bound = DescriptorExecutable::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(bound.recheck_original().is_err());
    }

    #[test]
    fn immutable_binding_is_captured_without_a_path_reread() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(&temp);
        let expected = b"#!/bin/sh\nexit 0\n";
        let bound = DescriptorExecutable::bind(&path).unwrap();
        let immutable = bound.immutable_binding();
        assert_eq!(immutable.bytes, expected.len() as u64);
        assert_eq!(immutable.sha256, sha256(expected));
        fs::remove_file(&path).unwrap();
        assert_eq!(bound.immutable_binding(), immutable);
    }

    #[test]
    fn build_artifact_accepts_cargo_like_hardlink_but_default_rejects_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(&temp);
        assert!(DescriptorExecutable::bind_build_artifact(&path).is_ok());
        let alias = temp.path().join("deps-kio-under-test");
        fs::hard_link(&path, &alias).unwrap();

        assert!(DescriptorExecutable::bind(&path).is_err());
        let bound = DescriptorExecutable::bind_build_artifact(&path).unwrap();
        assert!(bound.recheck_original().is_ok());

        fs::remove_file(&alias).unwrap();
        assert!(bound.recheck_original().is_err());
    }

    #[test]
    fn build_artifact_rejects_more_than_cargo_two_link_shape() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(&temp);
        fs::hard_link(&path, temp.path().join("deps-kio-under-test")).unwrap();
        fs::hard_link(&path, temp.path().join("unexpected-extra-alias")).unwrap();

        assert!(DescriptorExecutable::bind_build_artifact(&path).is_err());
    }

    #[test]
    fn build_artifact_rejects_in_place_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let path = executable(&temp);
        let alias = temp.path().join("deps-kio-under-test");
        fs::hard_link(&path, &alias).unwrap();
        let bound = DescriptorExecutable::bind_build_artifact(&path).unwrap();

        fs::write(&alias, b"#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(bound.recheck_original().is_err());
    }
}
