//! Mandatory Linux renderer resource scope. The fixed gate executes no renderer
//! until the parent has verified its inherited cgroup membership and limits.
use super::{ConfinementError, RenderSandbox, apply_unix_limits};
use crate::{
    BoundedProcessError as Error, BoundedProcessMonitor, BoundedProcessOptions,
    BoundedProcessOutput, UnixRendererMonitor,
};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus},
    time::{Duration, Instant},
};

const CLEANUP_GRACE: Duration = Duration::from_secs(5);
// Includes launcher, keeper, renderer processes AND their threads.
const TASKS: u64 = 64;
const CGROUP2_MAGIC: libc::c_long = 0x6367_7270;
const TOKEN: &[u8] = b"kio-render-start-v1\n";
const GATE: &str = r#"keep_fd=$1
ready_fd=$2
shift 2
(
    exec 0</dev/null 1>/dev/null 2>/dev/null
    IFS=' ' read -r keeper_pid rest </proc/self/stat || exit 125
    printf '%s\n' "$keeper_pid" >&"$ready_fd" || exit 125
    exec {ready_fd}>&-
    IFS= read -r -u "$keep_fd" keepalive
    exec {keep_fd}<&-
    exit 0
) &
exec {keep_fd}<&- {ready_fd}>&-
IFS= read -r gate || exit 125
[ "$gate" = kio-render-start-v1 ] || exit 125
exec 0</dev/null
exec "$@""#;

fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}
fn isolation(message: &'static str) -> Error {
    Error::Isolation(invalid(message))
}

pub(super) fn run<I, S>(
    sandbox: &RenderSandbox,
    args: I,
    environment: &[(OsString, OsString)],
    options: BoundedProcessOptions,
) -> Result<BoundedProcessOutput, ConfinementError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let started = Instant::now();
    let deadline = started
        .checked_add(options.timeout)
        .filter(|_| !options.timeout.is_zero())
        .ok_or_else(|| ConfinementError::Resource(invalid("invalid renderer wall timeout")))?;
    let cpu_usec = sandbox
        .limits
        .cpu_seconds
        .checked_mul(1_000_000)
        .filter(|value| *value != 0)
        .ok_or_else(|| ConfinementError::Resource(invalid("invalid aggregate CPU budget")))?;
    let memory = sandbox.limits.max_aggregate_memory_bytes;
    if memory == 0 || memory > i64::MAX as u64 {
        return Err(ConfinementError::Resource(invalid(
            "invalid aggregate memory budget",
        )));
    }
    let runtime_usec = u64::try_from(options.timeout.as_micros())
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| ConfinementError::Resource(invalid("invalid renderer wall timeout")))?;
    validate_mounts(sandbox)?;
    let systemd_run = trusted_helper("/usr/bin/systemd-run")?;
    let systemctl = trusted_helper("/usr/bin/systemctl")?;
    let shell = trusted_helper("/bin/bash")?;
    let bwrap = trusted_helper("/usr/bin/bwrap")?;
    let uid = unsafe { libc::geteuid() };
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    validate_runtime(&runtime, uid)?;
    let bus = runtime.join("bus");
    let manager_environment = vec![
        (OsString::from("XDG_RUNTIME_DIR"), runtime.into_os_string()),
        (
            OsString::from("DBUS_SESSION_BUS_ADDRESS"),
            OsString::from(format!("unix:path={}", bus.display())),
        ),
    ];
    let unit = unique_unit()?;
    #[cfg(test)]
    let unit = TEST_UNIT
        .with(|value| value.borrow().clone())
        .unwrap_or(unit);
    #[cfg(test)]
    let gate_override = TEST_GATE.with(|value| value.borrow().clone());
    #[cfg(test)]
    let gate_script = gate_override.as_deref().unwrap_or(GATE);
    #[cfg(not(test))]
    let gate_script = GATE;
    let cgroup_mount =
        open_directory(Path::new("/sys/fs/cgroup")).map_err(ConfinementError::Resource)?;
    verify_cgroup_fs(&cgroup_mount).map_err(ConfinementError::Resource)?;
    let scratch_file = open_directory(&sandbox.scratch).map_err(ConfinementError::Resource)?;
    let (keep_read, keep_write) = pipe()?;
    let (ready_read, ready_write) = pipe()?;
    // These descriptors remain owned until spawn. Only these exact high FDs
    // are inherited; no fixed slot can clobber Rust's private exec-error pipe.
    let child_keep = duplicate_at_least(&keep_read, 64)?;
    let child_ready = duplicate_at_least(&ready_write, 64)?;
    let keep_fd = child_keep.as_raw_fd();
    let ready_fd = child_ready.as_raw_fd();
    let mut monitor = Scope {
        unit,
        systemctl,
        manager_environment,
        mount: cgroup_mount,
        controls: None,
        keepalive: Some(keep_write),
        ready: ready_read,
        child_pipe_ends: Some(vec![keep_read, ready_write, child_keep, child_ready]),
        keeper_read_fd: keep_fd,
        keeper: None,
        deadline,
        runtime_usec,
        memory,
        cpu_usec,
        last_cpu_usec: std::cell::Cell::new(0),
        next_sample: Instant::now(),
        cleanup_deadline: None,
        cleaned: false,
        launched: false,
        scratch: UnixRendererMonitor::new(
            scratch_file,
            sandbox.scratch.clone(),
            sandbox.limits.max_file_bytes,
            0,
        ),
    };
    let mut command = Command::new(systemd_run);
    command
        .env_clear()
        .envs(monitor.manager_environment.iter().cloned())
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--expand-environment=no",
            "--no-ask-password",
        ])
        .arg(format!("--unit={}", monitor.unit));
    for property in [
        format!("MemoryMax={memory}"),
        "MemorySwapMax=0".into(),
        format!("TasksMax={TASKS}"),
        "CPUQuota=100%".into(),
        "CPUQuotaPeriodSec=100ms".into(),
        "OOMPolicy=kill".into(),
        format!("RuntimeMaxSec={runtime_usec}us"),
        "KillMode=control-group".into(),
        "KillSignal=SIGKILL".into(),
        "TimeoutStopSec=5s".into(),
    ] {
        command.arg(format!("--property={property}"));
    }
    command
        .arg("--")
        .arg(shell)
        .args(["-c", gate_script, "kio-render-gate"])
        .arg(keep_fd.to_string())
        .arg(ready_fd.to_string())
        .arg(bwrap);
    let bubblewrap = sandbox.linux_command(args)?;
    command.arg("--clearenv");
    for (name, value) in environment {
        if name.is_empty() || name.as_encoded_bytes().contains(&b'=') {
            return Err(ConfinementError::Resource(invalid(
                "invalid explicit renderer environment",
            )));
        }
        command.arg("--setenv").arg(name).arg(value);
    }
    command
        .args(bubblewrap.get_args())
        .current_dir(&sandbox.scratch);
    apply_unix_limits(&mut command, sandbox.limits)?;
    configure_helper(&mut command, Some((keep_fd, ready_fd)));
    monitor.launched = true;
    let result = crate::run_bounded_command_inner_impl(&mut command, options, None, &mut monitor);
    let cleanup = monitor.cleanup();
    cleanup?;
    result.map_err(ConfinementError::from)
}

fn pipe() -> Result<(File, File), ConfinementError> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(ConfinementError::Resource(io::Error::last_os_error()));
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}
fn duplicate_at_least(file: &File, minimum: i32) -> Result<File, ConfinementError> {
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, minimum) };
    if fd < 0 {
        return Err(ConfinementError::Resource(io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn configure_helper(command: &mut Command, pipes: Option<(i32, i32)>) {
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                || libc::getppid() != parent_pid
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            // Keep Rust's error pipe usable until exec; all unrelated inherited
            // descriptors close at exec. Only the two owned protocol pipes survive.
            if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some((keep, ready)) = pipes
                && (libc::fcntl(keep, libc::F_SETFD, 0) < 0
                    || libc::fcntl(ready, libc::F_SETFD, 0) < 0)
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
fn pidfd_alive(file: &File) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pfd, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result == 0)
}
fn overlap(path: &Path, control: &Path) -> bool {
    path.starts_with(control) || control.starts_with(path)
}
fn validate_mounts(sandbox: &RenderSandbox) -> Result<(), ConfinementError> {
    let sources: Vec<_> = std::iter::once(sandbox.program.clone())
        .chain(std::iter::once(sandbox.scratch.clone()))
        .chain(
            sandbox
                .runtime_mounts
                .iter()
                .map(|(source, _)| source.clone()),
        )
        .collect();
    for path in std::iter::once(&sandbox.program)
        .chain(std::iter::once(&sandbox.scratch))
        .chain(
            sandbox
                .runtime_mounts
                .iter()
                .flat_map(|(source, destination)| [source, destination]),
        )
    {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if ["/run", "/sys", "/proc", "/var/run"].iter().any(|control| {
            overlap(path, Path::new(control)) || overlap(&canonical, Path::new(control))
        }) {
            return Err(ConfinementError::Runtime(path.clone()));
        }
    }
    super::linux_mounts::validate_sources(&sources).map_err(ConfinementError::Resource)?;
    Ok(())
}
fn trusted_helper(path: &str) -> Result<PathBuf, ConfinementError> {
    let canonical = fs::canonicalize(path).map_err(ConfinementError::Resource)?;
    for ancestor in canonical.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(ConfinementError::Resource)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.file_type().is_symlink()
        {
            return Err(ConfinementError::Resource(invalid(
                "unsafe trusted renderer helper ancestry",
            )));
        }
    }
    let metadata = fs::metadata(&canonical).map_err(ConfinementError::Resource)?;
    if !metadata.is_file() || metadata.mode() & 0o6000 != 0 {
        return Err(ConfinementError::Resource(invalid(
            "privileged or nonregular renderer helper",
        )));
    }
    let name = CString::new(canonical.as_os_str().as_encoded_bytes())
        .map_err(|_| ConfinementError::Resource(invalid("invalid helper path")))?;
    let result = unsafe {
        libc::getxattr(
            name.as_ptr(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ENODATA) {
        return Err(ConfinementError::Resource(invalid(
            "renderer helper capabilities are present or unverifiable",
        )));
    }
    Ok(canonical)
}
fn validate_runtime(path: &Path, uid: u32) -> Result<(), ConfinementError> {
    use std::os::unix::fs::FileTypeExt;
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(ConfinementError::Resource)?;
        let owner = if ancestor == path { uid } else { 0 };
        if metadata.uid() != owner
            || !metadata.is_dir()
            || metadata.mode() & 0o022 != 0
            || (ancestor == path && metadata.mode() & 0o077 != 0)
        {
            return Err(ConfinementError::Resource(invalid(
                "unsafe user manager runtime directory",
            )));
        }
    }
    let metadata = fs::symlink_metadata(path.join("bus")).map_err(ConfinementError::Resource)?;
    if metadata.uid() != uid || !metadata.file_type().is_socket() {
        return Err(ConfinementError::Resource(invalid(
            "unsafe user manager bus",
        )));
    }
    Ok(())
}
fn unique_unit() -> Result<String, ConfinementError> {
    let mut random = [0_u8; 16];
    let count = unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) };
    if count != random.len() as isize {
        return Err(ConfinementError::Resource(invalid(
            "cannot allocate renderer scope identity",
        )));
    }
    Ok(format!(
        "kio-render-{}.scope",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
fn verify_cgroup_fs(file: &File) -> io::Result<()> {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if stat.f_type != CGROUP2_MAGIC {
        return Err(invalid("renderer scope is not on cgroup v2"));
    }
    Ok(())
}
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn open_beneath(root: &File, path: &Path, write: bool, directory: bool) -> io::Result<File> {
    let name = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| invalid("invalid cgroup path"))?;
    let flags = (if write {
        libc::O_WRONLY
    } else {
        libc::O_RDONLY
    }) | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | if directory { libc::O_DIRECTORY } else { 0 };
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: 0x08 | 0x04 | 0x02 | 0x01,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    verify_cgroup_fs(&file)?;
    if file.metadata()?.dev() != root.metadata()?.dev() {
        return Err(invalid("cgroup mount identity changed"));
    }
    Ok(file)
}
fn read_control(file: &File) -> io::Result<String> {
    let mut buffer = [0_u8; 8193];
    let count = file.read_at(&mut buffer, 0)?;
    if count == buffer.len() {
        return Err(invalid("oversized renderer control data"));
    }
    String::from_utf8(buffer[..count].to_vec())
        .map_err(|_| invalid("non-UTF8 renderer control data"))
}
fn key_values(text: &str, separator: char) -> io::Result<BTreeMap<&str, &str>> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once(separator)
            .ok_or_else(|| invalid("malformed renderer control data"))?;
        if key.is_empty() || values.insert(key, value).is_some() {
            return Err(invalid("ambiguous renderer control data"));
        }
    }
    Ok(values)
}
fn counter(file: &File, key: &str) -> io::Result<u64> {
    let text = read_control(file)?;
    let values = key_values(&text, ' ')?;
    let value = values
        .get(key)
        .ok_or_else(|| invalid("missing renderer resource counter"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("invalid renderer resource counter"));
    }
    value
        .parse()
        .map_err(|_| invalid("invalid renderer resource counter"))
}
fn duration_usec(text: &str) -> io::Result<u64> {
    let mut total = 0_u64;
    let mut previous_scale = u64::MAX;
    for part in text.split_whitespace() {
        let end = part
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| invalid("missing systemd duration unit"))?;
        let value: u64 = part[..end]
            .parse()
            .map_err(|_| invalid("invalid systemd duration"))?;
        let scale = match &part[end..] {
            "us" | "µs" => 1,
            "ms" => 1000,
            "s" => 1_000_000,
            "min" => 60_000_000,
            "h" => 3_600_000_000,
            _ => return Err(invalid("unexpected systemd duration unit")),
        };
        if scale >= previous_scale {
            return Err(invalid("ambiguous systemd duration"));
        }
        previous_scale = scale;
        total = total
            .checked_add(
                value
                    .checked_mul(scale)
                    .ok_or_else(|| invalid("systemd duration overflow"))?,
            )
            .ok_or_else(|| invalid("systemd duration overflow"))?;
    }
    if previous_scale == u64::MAX {
        return Err(invalid("missing systemd duration"));
    }
    Ok(total)
}

struct ManagerMonitor {
    deadline: Instant,
}
impl BoundedProcessMonitor for ManagerMonitor {
    fn observe(&mut self, _: &Child) -> Result<(), Error> {
        Ok(())
    }
    fn operation_deadline(&self, _: Instant) -> Instant {
        self.deadline
    }
    fn reap(&mut self, child: &mut Child) -> Result<ExitStatus, Error> {
        reap_before(child, self.deadline)
    }
}
fn reap_before(child: &mut Child, deadline: Instant) -> Result<ExitStatus, Error> {
    loop {
        if crate::unix_child_has_exited(child).map_err(Error::Wait)? {
            return child.wait().map_err(Error::Wait);
        }
        if Instant::now() >= deadline {
            return Err(Error::RendererCleanup(invalid(
                "renderer helper did not become waitable before its deadline",
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Controls {
    group: String,
    directory: File,
    kill: Option<File>,
    events: Option<File>,
    cpu: Option<File>,
    memory_events: Option<File>,
    pids_events: Option<File>,
}
// kernfs refreshes a retained removed directory's link count to subdirs+2.
// Neither st_nlink nor an unreadable accounting file proves cgroup emptiness.
fn scope_is_empty(mount: &File, controls: &Controls) -> io::Result<bool> {
    verify_cgroup_fs(&controls.directory)?;
    if controls
        .events
        .as_ref()
        .is_some_and(|events| counter(events, "populated").is_ok_and(|value| value == 0))
    {
        return Ok(true);
    }
    let relative = Path::new(&controls.group)
        .strip_prefix("/")
        .map_err(|_| invalid("invalid retained cgroup identity"))?;
    for (parent, path, directory) in [
        (mount, relative, true),
        (&controls.directory, Path::new("cgroup.events"), false),
    ] {
        match open_beneath(parent, path, false, directory) {
            Ok(_) => return Ok(false),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => (),
            Err(error) => return Err(error),
        }
    }
    let name = CString::new(format!("/proc/self/fd/{}", controls.directory.as_raw_fd()))
        .map_err(|_| invalid("invalid retained descriptor path"))?;
    let mut target = [0_u8; 8193];
    let count = unsafe { libc::readlink(name.as_ptr(), target.as_mut_ptr().cast(), target.len()) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize == target.len() {
        return Err(invalid("oversized retained cgroup descriptor link"));
    }
    let expected = format!("/sys/fs/cgroup{} (deleted)", controls.group);
    Ok(target[..count as usize] == *expected.as_bytes())
}

struct Scope {
    unit: String,
    systemctl: PathBuf,
    manager_environment: Vec<(OsString, OsString)>,
    mount: File,
    controls: Option<Controls>,
    keepalive: Option<File>,
    ready: File,
    child_pipe_ends: Option<Vec<File>>,
    keeper_read_fd: i32,
    keeper: Option<File>,
    deadline: Instant,
    runtime_usec: u64,
    memory: u64,
    cpu_usec: u64,
    last_cpu_usec: std::cell::Cell<u64>,
    next_sample: Instant,
    cleanup_deadline: Option<Instant>,
    cleaned: bool,
    launched: bool,
    scratch: UnixRendererMonitor,
}
impl Scope {
    fn manager(&self, args: &[&str], deadline: Instant) -> io::Result<BoundedProcessOutput> {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|value| !value.is_zero())
            .ok_or_else(|| invalid("renderer manager deadline expired"))?;
        let mut command = Command::new(&self.systemctl);
        command
            .env_clear()
            .envs(self.manager_environment.iter().cloned())
            .args(["--user", "--no-pager"])
            .args(args);
        configure_helper(&mut command, None);
        crate::run_bounded_command_inner_impl(
            &mut command,
            BoundedProcessOptions {
                timeout,
                max_stdout_bytes: 8192,
                max_stderr_bytes: 8192,
            },
            None,
            &mut ManagerMonitor { deadline },
        )
        .map_err(|_| invalid("bounded renderer manager query failed"))
    }
    fn anchor(&mut self, child: &Child) -> io::Result<bool> {
        use std::io::Read;
        let mut membership = String::new();
        File::open(format!("/proc/{}/cgroup", child.id()))?
            .take(8193)
            .read_to_string(&mut membership)?;
        if membership.len() > 8192 {
            return Err(invalid("oversized launcher cgroup membership"));
        }
        let Some(group) = membership
            .strip_prefix("0::")
            .and_then(|value| value.strip_suffix('\n'))
        else {
            return Err(invalid("invalid launcher cgroup membership"));
        };
        let relative = Path::new(group)
            .strip_prefix("/")
            .map_err(|_| invalid("nonabsolute scope cgroup"))?;
        // Do not touch a named unit until our still-owned PID actually belongs
        // to it. In particular, a unit-name collision grants no cleanup rights.
        if relative.file_name() != Some(OsStr::new(&self.unit)) {
            return Ok(false);
        }
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid("scope cgroup identity mismatch"));
        }
        let directory = open_beneath(&self.mount, relative, false, true)?;
        self.controls = Some(Controls {
            group: group.to_owned(),
            directory,
            kill: None,
            events: None,
            cpu: None,
            memory_events: None,
            pids_events: None,
        });
        let controls = self.controls.as_mut().expect("authority was retained");
        controls.events = Some(open_beneath(
            &controls.directory,
            Path::new("cgroup.events"),
            false,
            false,
        )?);
        controls.kill = Some(open_beneath(
            &controls.directory,
            Path::new("cgroup.kill"),
            true,
            false,
        )?);
        controls.cpu = Some(open_beneath(
            &controls.directory,
            Path::new("cpu.stat"),
            false,
            false,
        )?);
        controls.memory_events = Some(open_beneath(
            &controls.directory,
            Path::new("memory.events"),
            false,
            false,
        )?);
        controls.pids_events = Some(open_beneath(
            &controls.directory,
            Path::new("pids.events"),
            false,
            false,
        )?);
        for (name, expected) in [
            ("memory.max", self.memory.to_string()),
            ("memory.swap.max", "0".into()),
            ("pids.max", TASKS.to_string()),
            ("cpu.max", "100000 100000".into()),
            ("memory.oom.group", "1".into()),
            ("cgroup.type", "domain".into()),
        ] {
            if read_control(&open_beneath(
                &controls.directory,
                Path::new(name),
                false,
                false,
            )?)?
            .trim()
                != expected
            {
                return Err(invalid(
                    "actual renderer cgroup limits differ from requested limits",
                ));
            }
        }
        let output = self.manager(&["show", "--property=ControlGroup,ActiveState,KillMode,KillSignal,TimeoutStopUSec,RuntimeMaxUSec,OOMPolicy", &self.unit], self.deadline)?;
        if !output.status.success() {
            return Err(invalid("cannot verify owned scope fuse"));
        }
        let properties = key_values(&output.stdout, '=')?;
        if properties.get("ControlGroup") != Some(&group)
            || properties.get("ActiveState") != Some(&"active")
            || properties.get("KillMode") != Some(&"control-group")
            || properties.get("KillSignal") != Some(&"9")
            || properties.get("OOMPolicy") != Some(&"kill")
            || duration_usec(
                properties
                    .get("TimeoutStopUSec")
                    .ok_or_else(|| invalid("missing scope stop timeout"))?,
            )? != 5_000_000
            || duration_usec(
                properties
                    .get("RuntimeMaxUSec")
                    .ok_or_else(|| invalid("missing scope runtime fuse"))?,
            )? != self.runtime_usec
        {
            return Err(invalid("renderer scope fuse differs from requested limits"));
        }
        Ok(true)
    }
    fn keeper_alive(&self) -> Result<(), Error> {
        if self
            .keeper
            .as_ref()
            .is_some_and(|fd| pidfd_alive(fd).is_ok_and(|alive| alive))
        {
            Ok(())
        } else {
            Err(isolation(
                "renderer accounting keeper exited or became unverifiable",
            ))
        }
    }
    fn accept_keeper(&mut self, child: &Child) -> Result<(), Error> {
        let flags = unsafe { libc::fcntl(self.ready.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe {
                libc::fcntl(
                    self.ready.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            } < 0
        {
            return Err(Error::Isolation(io::Error::last_os_error()));
        }
        let mut record = Vec::new();
        loop {
            if Instant::now() >= self.deadline {
                return Err(isolation("renderer keeper readiness deadline expired"));
            }
            let mut bytes = [0_u8; 32];
            match self.ready.read(&mut bytes) {
                Ok(0) => return Err(isolation("renderer keeper closed readiness pipe")),
                Ok(count) => {
                    record.extend_from_slice(&bytes[..count]);
                    if record.len() > 16 {
                        return Err(isolation("oversized keeper readiness record"));
                    }
                    if record.ends_with(b"\n") {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => return Err(Error::Isolation(error)),
            }
        }
        let text =
            std::str::from_utf8(&record).map_err(|_| isolation("non-UTF8 keeper readiness"))?;
        let digits = text
            .strip_suffix('\n')
            .ok_or_else(|| isolation("invalid keeper readiness"))?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(isolation("invalid keeper PID"));
        }
        let pid: i32 = digits
            .parse()
            .map_err(|_| isolation("invalid keeper PID"))?;
        if pid <= 1 || pid as u32 == child.id() {
            return Err(isolation("invalid keeper PID"));
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Err(isolation(
                    "renderer accounting keeper exited before identity retention",
                ));
            }
            return Err(Error::Isolation(error));
        }
        self.keeper = Some(unsafe { File::from_raw_fd(fd as i32) });
        self.keeper_alive()?;
        let read_proc = |path: String| -> Result<String, Error> {
            let mut text = String::new();
            File::open(path)
                .and_then(|file| file.take(8193).read_to_string(&mut text))
                .map_err(Error::Isolation)?;
            if text.len() > 8192 {
                return Err(isolation("oversized keeper identity"));
            }
            Ok(text)
        };
        let direct_group = read_proc(format!("/proc/{}/cgroup", child.id()))?;
        let controls = self
            .controls
            .as_ref()
            .ok_or_else(|| isolation("missing admitted scope identity"))?;
        if direct_group != format!("0::{}\n", controls.group) {
            return Err(isolation(
                "launcher left its admitted scope before activation",
            ));
        }
        if read_proc(format!("/proc/{pid}/cgroup"))? != direct_group {
            return Err(isolation("keeper scope differs from owned launcher"));
        }
        let stat = read_proc(format!("/proc/{pid}/stat"))?;
        let fields = stat
            .rsplit_once(") ")
            .ok_or_else(|| isolation("invalid keeper process identity"))?
            .1;
        if fields
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u32>().ok())
            != Some(child.id())
        {
            return Err(isolation("keeper is not owned launcher's child"));
        }
        let pipe = fs::metadata(format!("/proc/{pid}/fd/{}", self.keeper_read_fd))
            .map_err(Error::Isolation)?;
        let owned = self
            .keepalive
            .as_ref()
            .ok_or_else(|| isolation("missing keepalive writer"))?
            .metadata()
            .map_err(Error::Isolation)?;
        if pipe.dev() != owned.dev()
            || pipe.ino() != owned.ino()
            || !std::os::unix::fs::FileTypeExt::is_fifo(&pipe.file_type())
        {
            return Err(isolation("keeper does not hold owned keepalive pipe"));
        }
        self.keeper_alive()
    }
    fn resources(&self) -> Result<(), Error> {
        let controls = self
            .controls
            .as_ref()
            .ok_or_else(|| isolation("missing renderer resource controls"))?;
        let read = |file: &Option<File>, key, resource| {
            let file = file.as_ref().ok_or_else(|| Error::ResourceAccounting {
                resource,
                source: invalid("missing retained resource counter"),
            })?;
            counter(file, key).map_err(|source| Error::ResourceAccounting { resource, source })
        };
        if read(&controls.memory_events, "oom", "memory")? != 0
            || read(&controls.memory_events, "oom_kill", "memory")? != 0
        {
            return Err(Error::AggregateResourceLimit { resource: "memory" });
        }
        if read(&controls.pids_events, "max", "task")? != 0 {
            return Err(Error::AggregateResourceLimit { resource: "task" });
        }
        let observed_usec = read(&controls.cpu, "usage_usec", "cpu")?;
        if observed_usec < self.last_cpu_usec.replace(observed_usec) {
            return Err(Error::ResourceAccounting {
                resource: "cpu",
                source: invalid("renderer aggregate CPU counter regressed"),
            });
        }
        if observed_usec > self.cpu_usec {
            return Err(Error::AggregateCpuLimit {
                limit_usec: self.cpu_usec,
                observed_usec,
            });
        }
        Ok(())
    }
    fn cleanup_inner(&mut self) -> io::Result<()> {
        if self.cleaned || !self.launched {
            return Ok(());
        }
        let deadline = *self
            .cleanup_deadline
            .get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
        if let Some(controls) = &mut self.controls {
            let kill_error = controls
                .kill
                .as_mut()
                .and_then(|kill| kill.write_all(b"1\n").err());
            self.keepalive.take();
            // Admission can fail after retaining identity but before kill was
            // opened. Group cancellation is secondary; still prove emptiness.
            loop {
                if scope_is_empty(&self.mount, controls)? {
                    self.cleaned = true;
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(kill_error.unwrap_or_else(|| {
                        invalid("renderer scope remained populated after cleanup grace")
                    }));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.keepalive.take();
        // Before authority is anchored only the trusted gate/keeper can run.
        // The caller kills its owned process group and EOF releases the keeper.
        // Never stop a unit by name: a colliding unit belongs to someone else.
        self.cleaned = true;
        Ok(())
    }
}
impl BoundedProcessMonitor for Scope {
    fn needs_startup_pipe(&self) -> bool {
        true
    }
    fn operation_deadline(&self, _: Instant) -> Instant {
        self.deadline
    }
    fn on_spawn(&mut self, child: &mut Child) -> Result<(), Error> {
        self.child_pipe_ends.take();
        loop {
            if Instant::now() >= self.deadline {
                return Err(isolation("renderer scope startup deadline expired"));
            }
            if crate::unix_child_has_exited(child).map_err(Error::Wait)? {
                return Err(isolation("renderer launcher exited before scope admission"));
            }
            if self.anchor(child).map_err(Error::Isolation)? {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.accept_keeper(child)?;
        self.resources()?;
        #[cfg(test)]
        if TEST_PAUSE_BEFORE_ACTIVATION.with(std::cell::Cell::get) {
            fs::write(
                self.scratch.scratch_path.join("phase"),
                b"before activation",
            )
            .map_err(Error::Isolation)?;
            loop {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(test)]
        if TEST_REFUSE.with(std::cell::Cell::get) {
            if scope_is_empty(
                &self.mount,
                self.controls.as_ref().expect("admitted test scope"),
            )
            .map_err(Error::Isolation)?
            {
                return Err(isolation("test live scope incorrectly considered empty"));
            }
            return Err(isolation("test refusal before renderer activation"));
        }
        if Instant::now() >= self.deadline {
            return Err(isolation("renderer deadline expired before activation"));
        }
        let mut gate = child
            .stdin
            .take()
            .ok_or_else(|| isolation("missing renderer activation pipe"))?;
        self.keeper_alive()?;
        gate.write_all(TOKEN).map_err(Error::Write)?;
        drop(gate);
        #[cfg(test)]
        if TEST_OBSERVE_RUNTIME_FUSE.with(std::cell::Cell::get) {
            let deadline = self.deadline + CLEANUP_GRACE;
            loop {
                if crate::unix_child_has_exited(child).map_err(Error::Wait)? {
                    let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    let result = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            child.id(),
                            &mut status,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    };
                    if result != 0
                        || status.si_code != libc::CLD_KILLED
                        || unsafe { status.si_status() } != libc::SIGKILL
                    {
                        return Err(isolation("test runtime fuse exit was not SIGKILL"));
                    }
                    let controls = self.controls.as_ref().expect("test admitted scope");
                    let empty = scope_is_empty(&self.mount, controls).map_err(Error::Isolation)?;
                    if empty {
                        return Err(isolation("test observed independent runtime fuse exit"));
                    }
                }
                if Instant::now() >= deadline {
                    return Err(isolation("test runtime fuse did not terminate renderer"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }
    fn observe(&mut self, child: &Child) -> Result<(), Error> {
        if Instant::now() >= self.next_sample {
            self.next_sample = Instant::now() + Duration::from_millis(50);
            self.resources()?;
        }
        if let Err(error) = self.keeper_alive() {
            // A whole-scope OOM can kill the keeper between scheduled samples.
            // Prefer an available kernel event; never infer OOM from its exit.
            self.resources()?;
            return Err(error);
        }
        self.scratch.observe(child)?;
        Ok(())
    }
    fn on_exit(&mut self, child: &Child) -> Result<(), Error> {
        let resources = self.resources().and_then(|()| self.keeper_alive());
        self.cleanup()?;
        self.scratch.next_scratch_observation = Instant::now();
        self.scratch.observe(child)?;
        resources
    }
    fn cleanup(&mut self) -> Result<(), Error> {
        self.cleanup_inner().map_err(Error::RendererCleanup)
    }
    fn reap(&mut self, child: &mut Child) -> Result<ExitStatus, Error> {
        let deadline = *self
            .cleanup_deadline
            .get_or_insert_with(|| Instant::now() + CLEANUP_GRACE);
        reap_before(child, deadline)
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if !self.cleaned
            && let Some(controls) = &mut self.controls
            && let Some(kill) = &mut controls.kill
        {
            let _ = kill.write_all(b"1\n");
        }
        self.keepalive.take();
    }
}

#[cfg(test)]
thread_local! {
    static TEST_UNIT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    static TEST_GATE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    static TEST_REFUSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_PAUSE_BEFORE_ACTIVATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_OBSERVE_RUNTIME_FUSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_ambiguous_and_overflowing_controls() {
        assert!(key_values("usage_usec 1\nusage_usec 2\n", ' ').is_err());
        for text in [
            "",
            "other 1\n",
            "usage_usec +1\n",
            "usage_usec -1\n",
            "usage_usec 18446744073709551616\n",
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(text.as_bytes()).unwrap();
            assert!(counter(&file, "usage_usec").is_err(), "{text:?}");
        }
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"usage_usec 123\nuser_usec 100\n").unwrap();
        assert_eq!(counter(&file, "usage_usec").unwrap(), 123);
    }
    #[test]
    fn bounded_duration_parser_accepts_actual_units_only() {
        for (text, expected) in [
            ("5s", 5_000_000),
            ("1min 2s 3ms 4us", 62_003_004),
            ("250µs", 250),
        ] {
            assert_eq!(duration_usec(text).unwrap(), expected);
        }
        for text in [
            "",
            " ",
            "infinity",
            "5",
            "5s 5s",
            "1ms 1s",
            "18446744073709551615h",
        ] {
            assert!(duration_usec(text).is_err(), "{text:?}");
        }
    }
    #[test]
    fn control_roots_and_parent_mounts_are_rejected() {
        let scratch = tempfile::tempdir().unwrap();
        for root in ["/", "/run", "/sys", "/proc", "/run/user"] {
            if !Path::new(root).exists() {
                continue;
            }
            let sandbox = RenderSandbox::new(
                Path::new("/bin/sh"),
                scratch.path(),
                [PathBuf::from(root)],
                super::super::RenderResourceLimits::default(),
            )
            .unwrap();
            assert!(validate_mounts(&sandbox).is_err(), "{root}");
        }
        let alias = scratch.path().join("control-alias");
        std::os::unix::fs::symlink("/run", &alias).unwrap();
        let sandbox = RenderSandbox::new(
            Path::new("/bin/sh"),
            scratch.path(),
            [alias],
            super::super::RenderResourceLimits::default(),
        )
        .unwrap();
        assert!(validate_mounts(&sandbox).is_err());
    }
    fn fixture(root: &Path) -> RenderSandbox {
        RenderSandbox::new(
            Path::new("/bin/sh"),
            root,
            ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
                .into_iter()
                .map(PathBuf::from)
                .filter(|path| path.exists()),
            super::super::RenderResourceLimits::default(),
        )
        .unwrap()
    }
    fn options() -> BoundedProcessOptions {
        BoundedProcessOptions {
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
        }
    }
    fn assert_scope_absent(unit: &str) {
        let uid = unsafe { libc::geteuid() };
        let deadline = Instant::now() + CLEANUP_GRACE;
        loop {
            let mut command = Command::new("/usr/bin/systemctl");
            command
                .env_clear()
                .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path=/run/user/{uid}/bus"),
                )
                .args(["--user", "show", "--property=LoadState,ControlGroup", unit]);
            configure_helper(&mut command, None);
            let output = crate::run_bounded_command_inner_impl(
                &mut command,
                options(),
                None,
                &mut ManagerMonitor { deadline },
            )
            .unwrap();
            let properties = key_values(&output.stdout, '=').unwrap();
            if properties.get("LoadState") == Some(&"not-found") {
                return;
            }
            assert!(
                output.status.success(),
                "scope query failed without proving absence: {}",
                output.stderr
            );
            if let Some(group) = properties.get("ControlGroup")
                && let Ok(relative) = Path::new(group).strip_prefix("/")
            {
                assert_eq!(relative.file_name(), Some(OsStr::new(unit)));
                if let Ok(events) = File::open(
                    Path::new("/sys/fs/cgroup")
                        .join(relative)
                        .join("cgroup.events"),
                ) && counter(&events, "populated").unwrap() == 0
                {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "owned scope remains active");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_refusal_after_keeper_ready_never_executes_renderer() {
        let scratch = tempfile::tempdir().unwrap();
        let unit = unique_unit().unwrap();
        TEST_UNIT.with(|value| *value.borrow_mut() = Some(unit.clone()));
        TEST_REFUSE.with(|value| value.set(true));
        let result =
            fixture(scratch.path()).run(["-c", "printf forbidden > marker"], &[], options());
        let _ = TEST_UNIT.with(|value| value.borrow_mut().take());
        TEST_REFUSE.with(|value| value.set(false));
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("test refusal before renderer activation"),
            "{error}"
        );
        assert!(!scratch.path().join("marker").exists());
        assert_scope_absent(&unit);
    }
    #[test]
    fn native_keeper_exit_before_activation_never_executes_renderer() {
        let scratch = tempfile::tempdir().unwrap();
        let unit = unique_unit().unwrap();
        TEST_UNIT.with(|value| *value.borrow_mut() = Some(unit.clone()));
        TEST_GATE.with(|value| {
            *value.borrow_mut() =
                Some(GATE.replace("    IFS= read -r -u \"$keep_fd\" keepalive", "    exit 0"))
        });
        let result =
            fixture(scratch.path()).run(["-c", "printf forbidden > marker"], &[], options());
        let _ = TEST_UNIT.with(|value| value.borrow_mut().take());
        let _ = TEST_GATE.with(|value| value.borrow_mut().take());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("keeper"), "{error}");
        assert!(!scratch.path().join("marker").exists());
        assert_scope_absent(&unit);
    }
    #[test]
    fn native_unit_collision_does_not_kill_existing_invocation() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let unit = unique_unit().unwrap();
        let existing_unit = unit.clone();
        let path = first.path().to_owned();
        let runner = std::thread::spawn(move || {
            TEST_UNIT.with(|value| *value.borrow_mut() = Some(existing_unit));
            fixture(&path).run(["-c", "printf ready > ready; while ! test -f stop; do /bin/sleep 0.02; done; printf survived"], &[], options())
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        while !first.path().join("ready").exists() {
            assert!(!runner.is_finished(), "first renderer could not start");
            assert!(
                Instant::now() < deadline,
                "first renderer readiness timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        TEST_UNIT.with(|value| *value.borrow_mut() = Some(unit.clone()));
        let collision =
            fixture(second.path()).run(["-c", "printf forbidden > marker"], &[], options());
        let _ = TEST_UNIT.with(|value| value.borrow_mut().take());
        assert!(collision.is_err());
        assert!(!second.path().join("marker").exists());
        assert!(!runner.is_finished(), "collision killed the existing unit");
        fs::write(first.path().join("stop"), b"stop").unwrap();
        let output = runner.join().unwrap().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, "survived");
        assert_scope_absent(&unit);
    }
    #[test]
    fn parent_death_protocol_helper() {
        let Ok(stage) = std::env::var("KIO_CGROUP_TEST_DEATH_STAGE") else {
            return;
        };
        let path = PathBuf::from(std::env::var_os("KIO_CGROUP_TEST_SCRATCH").unwrap());
        let unit = std::env::var("KIO_CGROUP_TEST_UNIT").unwrap();
        TEST_UNIT.with(|value| *value.borrow_mut() = Some(unit));
        match stage.as_str() {
            "before" => TEST_PAUSE_BEFORE_ACTIVATION.with(|value| value.set(true)),
            "after" => TEST_GATE.with(|value| {
                *value.borrow_mut() = Some(GATE.replace(
                    "exec 0</dev/null\nexec \"$@\"",
                    "printf phase > phase\nwhile :; do :; done",
                ))
            }),
            _ => panic!("invalid test death stage"),
        }
        let result = fixture(&path).run(["-c", "printf forbidden > marker"], &[], options());
        panic!("parent-death fixture returned unexpectedly: {result:?}");
    }
    #[test]
    fn native_parent_death_before_activation_and_before_bwrap_exec_cleans_scope() {
        for stage in ["before", "after"] {
            let scratch = tempfile::tempdir().unwrap();
            let unit = unique_unit().unwrap();
            let mut controller = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "confinement::linux_cgroup::tests::parent_death_protocol_helper",
                    "--nocapture",
                ])
                .env_clear()
                .env("KIO_CGROUP_TEST_DEATH_STAGE", stage)
                .env("KIO_CGROUP_TEST_SCRATCH", scratch.path())
                .env("KIO_CGROUP_TEST_UNIT", &unit)
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            while !scratch.path().join("phase").exists() {
                if crate::unix_child_has_exited(&controller).unwrap() {
                    let status = controller.wait().unwrap();
                    panic!("{stage} controller exited before the injection point: {status}");
                }
                if Instant::now() >= deadline {
                    controller.kill().unwrap();
                    let _ = reap_before(&mut controller, Instant::now() + CLEANUP_GRACE);
                    panic!("{stage} controller never reached the injection point");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            controller.kill().unwrap();
            reap_before(&mut controller, Instant::now() + CLEANUP_GRACE).unwrap();
            assert_scope_absent(&unit);
            assert!(!scratch.path().join("marker").exists());
        }
    }
    #[test]
    fn native_runtime_fuse_terminates_scope_without_monitor_cancellation() {
        let scratch = tempfile::tempdir().unwrap();
        let unit = unique_unit().unwrap();
        TEST_UNIT.with(|value| *value.borrow_mut() = Some(unit.clone()));
        TEST_OBSERVE_RUNTIME_FUSE.with(|value| value.set(true));
        let started = Instant::now();
        let result = fixture(scratch.path()).run(
            ["-c", "printf ready > ready; while :; do :; done"],
            &[],
            BoundedProcessOptions {
                timeout: Duration::from_secs(1),
                ..options()
            },
        );
        TEST_OBSERVE_RUNTIME_FUSE.with(|value| value.set(false));
        let _ = TEST_UNIT.with(|value| value.borrow_mut().take());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("test observed independent runtime fuse exit"),
            "{error}"
        );
        assert!(
            scratch.path().join("ready").is_file(),
            "renderer must have started before fuse expiry"
        );
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(started.elapsed() < Duration::from_secs(7));
        assert_scope_absent(&unit);
    }
}
