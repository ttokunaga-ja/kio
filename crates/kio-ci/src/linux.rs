//! The CI-only user-manager contract and its injected host regression tests.
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    time::Duration,
};
const ROOT: &str = "/sys/fs/cgroup";
const PREFIX: &str = "kio-confinement-preflight-";
const PROPERTIES: [&str; 9] = [
    "MemoryMax=64M",
    "MemorySwapMax=0",
    "TasksMax=32",
    "CPUQuota=100%",
    "OOMPolicy=kill",
    "RuntimeMaxSec=15s",
    "KillMode=control-group",
    "KillSignal=SIGKILL",
    "TimeoutStopSec=5s",
];
const TERMINATION: &str = "TimeoutStopUSec,KillSignal,KillMode,RuntimeMaxUSec";
type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Success,
    Exit(i32),
    Timeout,
}
#[derive(Clone, Debug)]
struct Output {
    status: Status,
    stdout: String,
    stderr: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    program: &'static str,
    args: Vec<String>,
    timeout: Duration,
}
impl Request {
    fn new(program: &'static str, args: &[&str], seconds: u64) -> Self {
        Self {
            program,
            args: args.iter().map(|s| s.to_string()).collect(),
            timeout: Duration::from_secs(seconds),
        }
    }
}
#[derive(Clone, Debug)]
struct Metadata {
    uid: u32,
    mode: u32,
    directory: bool,
    socket: bool,
}
trait Host {
    fn uid(&self) -> u32;
    fn metadata(&mut self, path: &Path) -> Result<Metadata>;
    fn canonical(&mut self, path: &Path) -> Result<PathBuf>;
    fn read(&mut self, path: &Path) -> Result<String>;
    fn run(&mut self, request: Request) -> Result<Output>;
    fn env_read(&mut self) -> Result<String>;
    fn env_append(&mut self, before: &str, suffix: &str) -> Result<()>;
    fn pause(&mut self);
}
fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn succeeded(output: Output) -> Result<String> {
    require(
        output.status == Status::Success,
        &format!("command {:?}: {}", output.status, output.stderr),
    )?;
    Ok(output.stdout)
}
fn valid_unit(unit: &str) -> bool {
    unit.strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(".scope"))
        .is_some_and(|s| {
            s.len() == 32
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}
fn membership_path(relative: &str, uid: u32, unit: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    require(valid_unit(unit) && uid != 0, "invalid probe UID/unit")?;
    require(
        relative.starts_with(&format!("/user.slice/user-{uid}.slice/user@{uid}.service/"))
            && path.file_name().is_some_and(|n| n == unit)
            && !relative.contains("//")
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && !relative.split('/').any(|p| p == "." || p == ".."),
        "probe did not enter its unique current-user scope",
    )?;
    Ok(Path::new(ROOT).join(relative.trim_start_matches('/')))
}
fn fields(text: &str) -> Result<BTreeMap<&str, &str>> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=').ok_or("malformed manager properties")?;
        require(
            map.insert(key, value).is_none(),
            "duplicate manager property",
        )?;
    }
    Ok(map)
}
fn probe(host: &mut impl Host, uid: u32, unit: &str) -> Result<String> {
    require(
        host.uid() == uid && uid != 0 && valid_unit(unit),
        "probe must remain unprivileged with exact UID/unit",
    )?;
    let membership = host.read(Path::new("/proc/self/cgroup"))?;
    let entries: Vec<_> = membership.lines().collect();
    require(
        entries.len() == 1,
        "probe must have a unified cgroup membership",
    )?;
    let relative = entries[0]
        .strip_prefix("0::")
        .ok_or("probe must have a unified cgroup membership")?;
    let cgroup = membership_path(relative, uid, unit)?;
    require(
        host.canonical(&cgroup)? == cgroup,
        "probe cgroup must be canonical within unified mount",
    )?;
    let parent = cgroup.parent().ok_or("missing cgroup parent")?;
    for filename in ["cgroup.controllers", "cgroup.subtree_control"] {
        let value = host.read(&parent.join(filename))?;
        require(
            ["cpu", "memory", "pids"]
                .iter()
                .all(|v| value.split_whitespace().any(|x| x == *v)),
            "required controllers are not delegated/enabled",
        )?;
    }
    for (filename, expected) in [
        ("memory.max", "67108864"),
        ("memory.swap.max", "0"),
        ("pids.max", "32"),
        ("memory.oom.group", "1"),
    ] {
        let value = host.read(&cgroup.join(filename))?;
        require(
            value.trim() == expected,
            &format!("{filename}: expected {expected}, got {}", value.trim()),
        )?;
    }
    let cpu = host.read(&cgroup.join("cpu.max"))?;
    let values: Vec<_> = cpu.split_whitespace().collect();
    require(
        values.len() == 2
            && values[0]
                .parse::<u64>()
                .is_ok_and(|q| q > 0 && values[1].parse::<u64>() == Ok(q)),
        "cpu.max must impose a 100% CPU quota",
    )?;
    let output = succeeded(host.run(Request::new(
        "systemctl",
        &["--user", "show", unit, &format!("--property={TERMINATION}")],
        5,
    ))?)?;
    let actual = fields(&output)?;
    let expected = BTreeMap::from([
        ("TimeoutStopUSec", "5s"),
        ("KillSignal", "9"),
        ("KillMode", "control-group"),
        ("RuntimeMaxUSec", "15s"),
    ]);
    require(
        actual == expected,
        "unexpected scope termination properties",
    )?;
    Ok(format!(
        "cgroup={}\nVerified resource controls and termination properties\n",
        cgroup.display()
    ))
}
fn cleanup(host: &mut impl Host, unit: &str, uid: u32, probe_output: &str) -> Result<()> {
    // Always try both operations, even if stop timed out or its transport failed.
    let stop = host.run(Request::new("systemctl", &["--user", "stop", unit], 10));
    let state = succeeded(host.run(Request::new(
        "systemctl",
        &[
            "--user",
            "show",
            unit,
            "--property=LoadState,ActiveState,ControlGroup",
        ],
        5,
    ))?)?;
    let state = fields(&state)?;
    require(
        state.len() == 3
            && state.contains_key("ActiveState")
            && state.contains_key("ControlGroup")
            && matches!(state.get("LoadState"), Some(&"loaded" | &"not-found")),
        "malformed cleanup LoadState properties",
    )?;
    let missing = state.get("LoadState") == Some(&"not-found");
    require(
        !missing
            || (state.get("ActiveState") == Some(&"inactive")
                && state.get("ControlGroup") == Some(&"")),
        "contradictory not-found scope state",
    )?;
    require(
        missing
            || (stop.is_ok_and(|r| r.status == Status::Success)
                && matches!(state.get("ActiveState"), Some(&"inactive" | &"failed"))),
        "could not clean up preflight scope",
    )?;
    let mut paths = Vec::new();
    if let Some(relative) = state.get("ControlGroup").filter(|s| !s.is_empty()) {
        paths.push(membership_path(relative, uid, unit)?);
    }
    for line in probe_output
        .lines()
        .filter_map(|s| s.strip_prefix("cgroup="))
    {
        let relative = line
            .strip_prefix(ROOT)
            .ok_or("invalid cleanup cgroup path")?;
        paths.push(membership_path(relative, uid, unit)?);
    }
    // A loaded unit with no known cgroup cannot prove that its children converged.
    require(missing || !paths.is_empty(), "cleanup lacks scope cgroup")?;
    paths.sort();
    paths.dedup();
    for path in paths {
        match host.canonical(&path) {
            Ok(canonical) => require(canonical == path, "cleanup cgroup is not canonical")?,
            Err(error) if error == "not-found" => continue,
            Err(error) => return Err(error),
        }
        let mut empty = false;
        // 50 bounded 100ms polls, independent of any child diagnostic output.
        for attempt in 0..=50 {
            match host.read(&path.join("cgroup.events")) {
                Ok(events) => {
                    let lines: Vec<_> = events.lines().filter_map(|s| s.split_once(' ')).collect();
                    let populated: Vec<_> =
                        lines.iter().filter(|(k, _)| *k == "populated").collect();
                    require(
                        populated.len() == 1 && matches!(populated[0].1, "0" | "1"),
                        "malformed cgroup populated state",
                    )?;
                    if populated[0].1 == "0" {
                        empty = true;
                        break;
                    }
                }
                Err(error) if error == "not-found" => {
                    empty = true;
                    break;
                }
                Err(error) => return Err(error),
            }
            if attempt == 50 {
                break;
            }
            host.pause();
        }
        require(empty, "preflight scope still has processes after stop")?;
    }
    Ok(())
}
// GITHUB_ENV heredoc payloads are data, never independent environment entries.
fn environment_entries<'a>(text: &'a str, key: &str) -> Result<Vec<&'a str>> {
    let mut entries = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some((name, delimiter)) = line
            .split_once("<<")
            .filter(|(name, _)| !name.contains('='))
        {
            require(
                !delimiter.is_empty(),
                "empty GITHUB_ENV multiline delimiter",
            )?;
            if name == key {
                entries.push(line);
            }
            require(
                lines.by_ref().any(|value| value == delimiter),
                "unterminated GITHUB_ENV multiline entry",
            )?;
        } else if line.starts_with(&format!("{key}=")) {
            entries.push(line);
        }
    }
    Ok(entries)
}
fn preflight(host: &mut impl Host, unit: &str, executable: &str) -> Result<()> {
    let uid = host.uid();
    require(
        uid != 0 && valid_unit(unit),
        "requires nonroot current UID and unique unit",
    )?;
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    require(
        host.canonical(&runtime)? == runtime,
        "runtime directory is not canonical",
    )?;
    let metadata = host.metadata(&runtime)?;
    require(
        metadata.directory && metadata.uid == uid && metadata.mode == 0o700,
        "runtime directory must be runner-owned with mode 0700",
    )?;
    let bus = host.metadata(&runtime.join("bus"))?;
    require(
        bus.socket && bus.uid == uid,
        "user manager bus is not a runner-owned socket",
    )?;
    let filesystem = succeeded(host.run(Request::new("stat", &["-f", "-c", "%T", ROOT], 5))?)?;
    require(
        filesystem.trim() == "cgroup2fs",
        "unified cgroup v2 is required",
    )?;
    succeeded(host.run(Request::new(
        "systemctl",
        &["--user", "show-environment"],
        10,
    ))?)?;
    succeeded(host.run(Request::new(
        "busctl",
        &[
            "--user",
            "--timeout=10",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.DBus.Peer",
            "Ping",
        ],
        15,
    ))?)?;
    let mut request = Request::new(
        "systemd-run",
        &[
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            &format!("--unit={unit}"),
        ],
        20,
    );
    request
        .args
        .extend(PROPERTIES.iter().map(|v| format!("--property={v}")));
    request.args.extend([
        "--".into(),
        executable.into(),
        "--internal-probe".into(),
        uid.to_string(),
        unit.into(),
    ]);
    let result = host.run(request);
    let stdout = result
        .as_ref()
        .map(|r| r.stdout.as_str())
        .unwrap_or_default();
    let cleaned = cleanup(host, unit, uid, stdout);
    cleaned?;
    let output = succeeded(result?)?;
    require(
        output
            .lines()
            .any(|s| s == "Verified resource controls and termination properties"),
        "probe did not report verified controls",
    )?;
    let before = host.env_read()?;
    let mut suffix = String::new();
    for (key, value) in [
        ("XDG_RUNTIME_DIR", runtime.display().to_string()),
        (
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/bus", runtime.display()),
        ),
    ] {
        let expected = format!("{key}={value}");
        let existing = environment_entries(&before, key)?;
        require(
            existing.is_empty() || existing == [expected.as_str()],
            &format!("conflicting {key} in GITHUB_ENV"),
        )?;
        if existing.is_empty() {
            suffix.push_str(&expected);
            suffix.push('\n');
        }
    }
    if !suffix.is_empty() {
        if !before.is_empty() && !before.ends_with('\n') {
            suffix.insert(0, '\n');
        }
        host.env_append(&before, &suffix)?;
    }
    Ok(())
}
#[cfg(any(target_os = "linux", all(unix, test)))]
mod native;
#[cfg(target_os = "linux")]
pub(super) fn entry(args: Vec<String>) -> Result<()> {
    native::entry(args)
}
#[cfg(test)]
mod tests;
