//! Fixed native host operations. No shell, caller-selected command, or sudo.
use super::*;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
        process::CommandExt,
    },
    process::{Child, Command, Stdio},
    thread,
    time::Instant,
};
const MAX_BYTES: usize = 64 * 1024;
struct Native {
    uid: u32,
    envfile: PathBuf,
    executable: String,
    env_identity: Option<(u64, u64)>,
}
fn read_file(path: &Path) -> Result<String> {
    let file = File::open(path).map_err(io_error)?;
    read_bounded(file)
}
fn io_error(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        "not-found".into()
    } else {
        error.to_string()
    }
}
fn read_bounded(file: File) -> Result<String> {
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    require(bytes.len() <= MAX_BYTES, "file exceeds bounded capture")?;
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8".into())
}
impl Native {
    fn new() -> Result<Self> {
        require(
            env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
                && env::var("RUNNER_ENVIRONMENT").as_deref() == Ok("github-hosted")
                && env::var("RUNNER_OS").as_deref() == Ok("Linux"),
            "requires GitHub-hosted Linux runner",
        )?;
        // SAFETY: getuid/geteuid have no arguments or mutable memory access.
        let uid = unsafe { libc::getuid() };
        require(
            uid != 0 && uid == unsafe { libc::geteuid() },
            "requires unprivileged current UID",
        )?;
        for key in ["GITHUB_RUN_ID", "GITHUB_WORKSPACE", "RUNNER_TEMP"] {
            require(
                env::var_os(key).is_some_and(|v| !v.is_empty()),
                "GitHub runner context is incomplete",
            )?;
        }
        let executable = fs::canonicalize(env::current_exe().map_err(io_error)?)
            .map_err(io_error)?
            .into_os_string()
            .into_string()
            .map_err(|_| "non-UTF8 executable")?;
        let envfile = env::var_os("GITHUB_ENV")
            .filter(|v| !v.is_empty())
            .ok_or("GITHUB_ENV is unavailable")?
            .into();
        let mut host = Self {
            uid,
            envfile,
            executable,
            env_identity: None,
        };
        // Prove the existing export file is regular and writable before a scope.
        let metadata = host.env_open()?.metadata().map_err(io_error)?;
        host.env_identity = Some((metadata.dev(), metadata.ino()));
        Ok(host)
    }
    fn env_open(&self) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.envfile)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        require(
            metadata.is_file() && metadata.uid() == self.uid && metadata.nlink() == 1,
            "GITHUB_ENV must be an existing runner-owned writable regular file",
        )?;
        require(
            self.env_identity
                .is_none_or(|identity| identity == (metadata.dev(), metadata.ino())),
            "GITHUB_ENV identity changed before export",
        )?;
        Ok(file)
    }
}
impl Host for Native {
    fn uid(&self) -> u32 {
        self.uid
    }
    fn metadata(&mut self, path: &Path) -> Result<Metadata> {
        let m = fs::symlink_metadata(path).map_err(io_error)?;
        Ok(Metadata {
            uid: m.uid(),
            mode: m.mode() & 0o7777,
            directory: m.is_dir(),
            socket: m.file_type().is_socket(),
        })
    }
    fn canonical(&mut self, path: &Path) -> Result<PathBuf> {
        fs::canonicalize(path).map_err(io_error)
    }
    fn read(&mut self, path: &Path) -> Result<String> {
        read_file(path)
    }
    fn run(&mut self, request: Request) -> Result<Output> {
        let executable = match request.program {
            "stat" => "/usr/bin/stat",
            "systemctl" => "/usr/bin/systemctl",
            "busctl" => "/usr/bin/busctl",
            "systemd-run" => "/usr/bin/systemd-run",
            _ => return Err("unknown fixed command".into()),
        };
        let runtime = format!("/run/user/{}", self.uid);
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .args(request.args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={runtime}/bus"),
            );
        // The same fixed helper child needs only these ephemeral runner guards.
        // Drop ambient systemd routing/config overrides and unrelated secrets.
        for key in [
            "GITHUB_ACTIONS",
            "RUNNER_ENVIRONMENT",
            "RUNNER_OS",
            "GITHUB_RUN_ID",
            "GITHUB_WORKSPACE",
            "RUNNER_TEMP",
            "GITHUB_ENV",
        ] {
            if let Some(value) = env::var_os(key) {
                command.env(key, value);
            }
        }
        let discard_stdout = request.program == "busctl"
            || (request.program == "systemctl"
                && command
                    .get_args()
                    .eq(["--user", "show-environment"].map(std::ffi::OsStr::new)));
        capture(command, request.timeout, discard_stdout)
    }
    fn env_read(&mut self) -> Result<String> {
        read_bounded(self.env_open()?)
    }
    fn env_append(&mut self, before: &str, suffix: &str) -> Result<()> {
        let mut file = self.env_open()?;
        // Serialize competing cooperating writers and re-read before appending.
        // SAFETY: file owns this valid descriptor for the entire lock/read/write.
        require(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "GITHUB_ENV is locked by another writer",
        )?;
        let current = read_bounded(file.try_clone().map_err(io_error)?)?;
        require(current == before, "GITHUB_ENV changed before export")?;
        let held = file.metadata().map_err(io_error)?;
        let named = fs::symlink_metadata(&self.envfile).map_err(io_error)?;
        require(
            named.is_file() && named.dev() == held.dev() && named.ino() == held.ino(),
            "GITHUB_ENV was replaced before export",
        )?;
        file.write_all(suffix.as_bytes()).map_err(io_error)
    }
    fn pause(&mut self) {
        thread::sleep(Duration::from_millis(100));
    }
}
struct Group {
    child: Child,
    armed: bool,
}
impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: spawn created a fresh process group with this positive PID.
        // Negative PID targets only that owned group, never the runner group.
        if self.armed {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            // An owned launcher may have left its initial process group.
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}
fn nonblocking(file: &impl AsRawFd) -> Result<()> {
    // SAFETY: fcntl accesses only this live pipe descriptor.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    require(
        flags >= 0
            && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == 0,
        "cannot bound process pipe reads",
    )
}
fn drain(file: &mut impl Read, bytes: &mut Vec<u8>) -> Result<bool> {
    let mut buffer = [0u8; 4096];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                require(
                    bytes.len() + n <= MAX_BYTES,
                    "command output exceeds bounded capture",
                )?;
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(e)),
        }
    }
}
fn capture(mut command: Command, timeout: Duration, discard_stdout: bool) -> Result<Output> {
    let child = command
        .stdin(Stdio::null())
        .stdout(if discard_stdout {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(io_error)?;
    let mut group = Group { child, armed: true };
    let mut stdout = group.child.stdout.take();
    let mut stderr = group.child.stderr.take().ok_or("missing stderr pipe")?;
    if let Some(pipe) = &stdout {
        nonblocking(pipe)?;
    }
    nonblocking(&stderr)?;
    let deadline = Instant::now() + timeout;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = loop {
        let out_done = match &mut stdout {
            Some(pipe) => drain(pipe, &mut out)?,
            None => true,
        };
        let err_done = drain(&mut stderr, &mut err)?;
        if out_done
            && err_done
            && let Some(status) = group.child.try_wait().map_err(io_error)?
        {
            group.armed = false;
            break if status.success() {
                Status::Success
            } else {
                Status::Exit(status.code().unwrap_or(-1))
            };
        }
        if Instant::now() >= deadline {
            // Kill the owned launcher group; caller still stops its systemd scope.
            unsafe {
                libc::kill(-(group.child.id() as i32), libc::SIGKILL);
            }
            let _ = group.child.kill();
            let reap_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                if group.child.try_wait().map_err(io_error)?.is_some() {
                    group.armed = false;
                    break;
                }
                require(
                    Instant::now() < reap_deadline,
                    "owned command did not terminate after SIGKILL",
                )?;
                thread::sleep(Duration::from_millis(10));
            }
            break Status::Timeout;
        }
        thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: String::from_utf8(out).map_err(|_| "command stdout is not UTF-8")?,
        stderr: String::from_utf8(err).map_err(|_| "command stderr is not UTF-8")?,
    })
}
fn parse_cli(args: &[String], uid: u32) -> Result<bool> {
    match args {
        [] => Ok(false),
        [mode, raw_uid, unit]
            if mode == "--internal-probe"
                && raw_uid == &uid.to_string()
                && uid != 0
                && valid_unit(unit) =>
        {
            Ok(true)
        }
        _ => Err("expected no arguments or fixed internal probe UID/unit".into()),
    }
}
pub(super) fn entry(args: Vec<String>) -> Result<()> {
    let mut host = Native::new()?;
    if parse_cli(&args, host.uid)? {
        let output = probe(
            &mut host,
            args[1].parse().map_err(|_| "invalid UID")?,
            &args[2],
        )?;
        print!("{output}");
        return Ok(());
    }
    // Kernel entropy gives each invocation ownership of one unpredictable unit.
    let mut random = [0u8; 16];
    File::open("/dev/urandom")
        .map_err(io_error)?
        .read_exact(&mut random)
        .map_err(io_error)?;
    let unit = format!(
        "{PREFIX}{}.scope",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let executable = host.executable.clone();
    preflight(&mut host, &unit, &executable)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_cli_refuses_overrides_and_noncanonical_uid() {
        let _entry: fn(Vec<String>) -> Result<()> = entry;
        assert_eq!(parse_cli(&[], 1001), Ok(false));
        assert_eq!(
            parse_cli(
                &[
                    "--internal-probe".into(),
                    "1001".into(),
                    super::super::tests::UNIT.into()
                ],
                1001
            ),
            Ok(true)
        );
        for args in [
            vec!["--help".into()],
            vec![
                "--internal-probe".into(),
                "01001".into(),
                super::super::tests::UNIT.into(),
            ],
            vec![
                "--internal-probe".into(),
                "0".into(),
                super::super::tests::UNIT.into(),
            ],
        ] {
            assert!(parse_cli(&args, 1001).is_err());
        }
    }
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }
    fn host_for_file(path: &Path) -> Native {
        let metadata = fs::metadata(path).unwrap();
        Native {
            uid: metadata.uid(),
            envfile: path.into(),
            executable: String::new(),
            env_identity: Some((metadata.dev(), metadata.ino())),
        }
    }
    #[test]
    fn identical_content_replacement_refused_before_export() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("env");
        fs::write(&path, "existing=1\n").unwrap();
        let mut host = host_for_file(&path);
        let before = host.env_read().unwrap();
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, &before).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(
            host.env_append(&before, "new=2\n")
                .unwrap_err()
                .contains("identity changed")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
    #[test]
    fn export_file_symlink_hardlink_and_nonregular_refused() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        fs::write(&original, "").unwrap();
        let mut host = host_for_file(&original);
        host.env_read().unwrap();
        host.env_append("", "new=2\n").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        host.envfile = link;
        assert!(host.env_open().is_err());
        fs::hard_link(&original, directory.path().join("alias")).unwrap();
        host.envfile = original;
        assert!(host.env_open().is_err());
        host.envfile = directory.path().into();
        assert!(host.env_open().is_err());
    }
    #[test]
    fn group_escape_child_fixture() {
        if env::var_os("KIO_CI_TEST_GROUP_ESCAPE").is_none() {
            return;
        }
        // SAFETY: test-only child joins its parent's existing group in this session.
        assert_eq!(
            unsafe { libc::setpgid(0, libc::getpgid(libc::getppid())) },
            0
        );
        println!("escaped-group");
        std::io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(10));
    }
    #[test]
    fn timeout_kills_and_reaps_owned_pid_after_group_escape() {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "linux::native::tests::group_escape_child_fixture",
                "--nocapture",
            ])
            .env("KIO_CI_TEST_GROUP_ESCAPE", "1");
        let output = capture(command, Duration::from_millis(300), false).unwrap();
        assert_eq!(output.status, Status::Timeout);
        assert!(output.stdout.contains("escaped-group"));
    }
    #[test]
    fn captures_failure_without_unbounded_output() {
        let output = capture(
            shell("printf stdout; printf stderr >&2; exit 7"),
            Duration::from_secs(1),
            false,
        )
        .unwrap();
        assert_eq!(output.status, Status::Exit(7));
        assert_eq!(output.stdout, "stdout");
        assert_eq!(output.stderr, "stderr");
    }
    #[test]
    fn health_stdout_is_discarded_without_capture() {
        let output = capture(
            shell("printf secret_environment"),
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        assert_eq!(output.status, Status::Success);
        assert!(output.stdout.is_empty());
    }
    #[test]
    fn output_overflow_is_refused() {
        assert!(
            capture(
                shell("while :; do printf '1234567890123456789012345678901234567890'; done"),
                Duration::from_secs(1),
                false
            )
            .unwrap_err()
            .contains("bounded capture")
        );
    }
    #[test]
    fn timeout_bounds_owned_command_and_inherited_pipe() {
        let start = Instant::now();
        let output = capture(shell("sleep 10 & wait"), Duration::from_millis(50), false).unwrap();
        assert_eq!(output.status, Status::Timeout);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
