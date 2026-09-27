#![cfg_attr(all(test, not(target_os = "macos")), allow(dead_code))]
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, Metadata},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, String>;
const DEVELOPER: &str = "/Applications/Xcode_26.5.app/Contents/Developer";
const SDKS: &str =
    "/Applications/Xcode_26.5.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs";
const LIMIT: u64 = 65536;
fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|e| e.to_string())
}
fn cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity(u64, u64, u32);
fn identity(m: &Metadata) -> Identity {
    Identity(m.dev(), m.ino(), m.mode() & 0o170000)
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Directory,
    File,
    Link(PathBuf),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    id: Identity,
    kind: Kind,
    shared: bool,
    size: u64,
}
type Plan = BTreeMap<PathBuf, Entry>;
fn kind(m: &Metadata) -> Result<Kind> {
    source_kind(m, false)
}
fn source_kind(m: &Metadata, allow_shared: bool) -> Result<Kind> {
    require(
        m.mode() & 0o7000 == 0,
        "special permission bits on SDK entry",
    )?;
    if m.is_dir() {
        Ok(Kind::Directory)
    } else if m.is_file() {
        require(allow_shared || m.nlink() == 1, "hardlinked SDK file")?;
        Ok(Kind::File)
    } else if m.is_symlink() {
        Ok(Kind::Link(PathBuf::new()))
    } else {
        Err("unsupported SDK entry type".into())
    }
}
fn check_link(path: &Path, root: &Path) -> Result<PathBuf> {
    let target = io(fs::read_link(path))?;
    require(
        !target.as_os_str().is_empty()
            && !target.is_absolute()
            && !target
                .as_os_str()
                .as_bytes()
                .iter()
                .any(|b| b"\0\r\n".contains(b)),
        "unsafe SDK symlink target",
    )?;
    let mut cursor = path.parent().ok_or("link has no parent")?.to_path_buf();
    for part in target.components() {
        match part {
            Component::ParentDir => {
                cursor.pop();
            }
            Component::CurDir => {}
            Component::Normal(p) => cursor.push(p),
            _ => return Err("unsafe link component".into()),
        }
        require(cursor.starts_with(root), "SDK symlink escapes tree")?;
    }
    let resolved = io(fs::canonicalize(path))?;
    require(resolved.starts_with(root), "SDK symlink escapes tree")?;
    let m = io(fs::metadata(&resolved))?;
    require(m.is_dir() || m.is_file(), "unsupported symlink destination")?;
    Ok(target)
}
fn plan_tree(root: &Path) -> Result<Plan> {
    plan_tree_with_shared(root, false)
}
fn plan_tree_with_shared(root: &Path, allow_shared: bool) -> Result<Plan> {
    fn visit(path: &Path, root: &Path, plan: &mut Plan, allow_shared: bool) -> Result<()> {
        let m = io(fs::symlink_metadata(path))?;
        let mut k = source_kind(&m, allow_shared)
            .map_err(|e| format!("{e}: {} (nlink={})", path.display(), m.nlink()))?;
        if matches!(k, Kind::Link(_)) {
            k = Kind::Link(check_link(path, root)?);
        }
        let directory = k == Kind::Directory;
        plan.insert(
            path.to_path_buf(),
            Entry {
                id: identity(&m),
                kind: k,
                shared: m.is_file() && m.nlink() > 1,
                size: if m.is_file() { m.len() } else { 0 },
            },
        );
        if directory {
            let mut children = io(fs::read_dir(path))?
                .map(|e| e.map(|e| e.path()))
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?;
            children.sort();
            for child in children {
                visit(&child, root, plan, allow_shared)?;
            }
        }
        Ok(())
    }
    let mut plan = Plan::new();
    visit(root, root, &mut plan, allow_shared)?;
    Ok(plan)
}
fn open_node(path: &Path) -> Result<File> {
    require(path.is_absolute(), "node path must be absolute")?;
    let mut file = io(File::open("/"))?;
    for part in path.components().skip(1) {
        let Component::Normal(part) = part else {
            return Err("invalid node component".into());
        };
        let name = CString::new(part.as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: parent descriptor and NUL-terminated component remain live.
        let fd = unsafe {
            libc::openat(
                file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        require(
            fd >= 0,
            &format!(
                "cannot open SDK component: {}",
                std::io::Error::last_os_error()
            ),
        )?;
        // SAFETY: openat returned a new owned descriptor.
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}
fn bounded_metadata(path: &Path) -> Result<Vec<u8>> {
    let file = open_node(path)?;
    require(io(file.metadata())?.is_file(), "metadata is not a file")?;
    let mut data = Vec::new();
    io(file.take(LIMIT + 1).read_to_end(&mut data))?;
    require(data.len() as u64 <= LIMIT, "oversized SDK metadata")?;
    Ok(data)
}
fn metadata(settings: &Value, system: &Value) -> Result<()> {
    require(
        settings["CanonicalName"] == "macosx26.5"
            && settings["Version"] == "26.5"
            && system["ProductName"] == "macOS"
            && system["ProductVersion"] == "26.5"
            && system["ProductBuildVersion"] == "25F70",
        "SDK metadata mismatch",
    )
}
fn command_spec(program: &str, args: &[&str], developer: bool) -> Command {
    let mut c = Command::new(program);
    c.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if developer {
        c.env("DEVELOPER_DIR", DEVELOPER);
    }
    c
}
fn command(program: &str, args: &[&str], developer: bool, input: &[u8]) -> Result<String> {
    let mut child = io(command_spec(program, args, developer).spawn())?;
    let mut stdin = child.stdin.take().ok_or("missing stdin")?;
    let data = input.to_vec();
    let (write_tx, write_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = write_tx.send(stdin.write_all(&data));
    });
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut data = Vec::new();
        let result = stdout.take(LIMIT + 1).read_to_end(&mut data).map(|_| data);
        let _ = read_tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            other => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("SDK command timeout or wait failure: {other:?}"));
            }
        }
    };
    require(status.success(), "SDK command failed")?;
    io(write_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "stdin writer timeout or failure")?)?;
    let data = io(read_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "stdout reader timeout or failure")?)?;
    require(data.len() as u64 <= LIMIT, "oversized command output")?;
    String::from_utf8(data)
        .map(|s| s.trim().to_owned())
        .map_err(|e| e.to_string())
}
fn read_metadata(root: &Path) -> Result<()> {
    let settings: Value =
        serde_json::from_slice(&bounded_metadata(&root.join("SDKSettings.json"))?)
            .map_err(|e| e.to_string())?;
    let plist = bounded_metadata(&root.join("System/Library/CoreServices/SystemVersion.plist"))?;
    let system: Value = serde_json::from_str(&command(
        "/usr/bin/plutil",
        &["-convert", "json", "-o", "-", "-"],
        false,
        &plist,
    )?)
    .map_err(|e| e.to_string())?;
    metadata(&settings, &system)?;
    println!(
        "{}",
        serde_json::json!({"SDKSettings":settings,"SystemVersion":system})
    );
    Ok(())
}
fn sdk_path(text: &str, sdks: &Path) -> Result<PathBuf> {
    let path = Path::new(text);
    fn valid(path: &Path, sdks: &Path) -> bool {
        path.parent() == Some(sdks)
            && matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("MacOSX.sdk" | "MacOSX26.5.sdk")
            )
    }
    require(
        path.is_absolute() && !text.bytes().any(|b| b"\0\r\n".contains(&b)) && valid(path, sdks),
        "SDK is outside the pinned installation",
    )?;
    let resolved = io(fs::canonicalize(path))?;
    require(
        valid(&resolved, sdks),
        "canonical SDK is outside the pinned installation",
    )?;
    Ok(resolved)
}
fn discover(developer: bool) -> Result<(PathBuf, Vec<String>)> {
    let values = [
        "--show-sdk-path",
        "--show-sdk-version",
        "--show-sdk-build-version",
    ]
    .iter()
    .map(|flag| {
        command(
            "/usr/bin/xcrun",
            &["--no-cache", "--sdk", "macosx26.5", flag],
            developer,
            &[],
        )
    })
    .collect::<Result<Vec<_>>>()?;
    discovery_identity(&values[1], &values[2])?;
    Ok((sdk_path(&values[0], Path::new(SDKS))?, values))
}
fn discovery_identity(version: &str, build: &str) -> Result<()> {
    require(
        version == "26.5" && build == "25F70",
        "xcrun SDK version/build mismatch",
    )
}
fn trusted(path: &Path, uid: u32, gid: u32, mode: u32) -> bool {
    uid == 0
        && mode & 0o002 == 0
        && (mode & 0o020 == 0
            || (path == Path::new("/Applications") && gid == 80 && mode & 0o7777 == 0o775))
}
#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "macos")]
pub use darwin::prepare;
mod detach;
#[cfg(test)]
mod tests;
trait Operations {
    fn inspect(&self, path: &Path, expected: Option<&Identity>, mutate: bool) -> Result<()>;
    fn metadata(&self, root: &Path) -> Result<()>;
    fn link(&self, path: &Path, entry: &Entry, mutate: bool, require_root: bool) -> Result<()>;
    fn detach(&self, path: &Path, entry: &Entry, parent: &Entry) -> Result<Identity>;
    fn select(&self) -> Result<()>;
}
fn normalize(root: &Path, ops: &impl Operations) -> Result<usize> {
    // Root and /Applications are checks only, never mutation targets.
    for p in ["/", "/Applications"] {
        ops.inspect(Path::new(p), None, false)?;
    }
    let mut ancestors: Vec<_> = root
        .ancestors()
        .skip(1)
        .filter(|p| *p != Path::new("/") && *p != Path::new("/Applications"))
        .collect();
    ancestors.reverse();
    let ancestors = ancestors
        .into_iter()
        .map(|p| {
            let m = io(fs::symlink_metadata(p))?;
            require(
                kind(&m)? == Kind::Directory,
                "symlink/non-directory SDK ancestor",
            )?;
            Ok((p, identity(&m)))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut plan = plan_tree_with_shared(root, true)?;
    let shared_count = plan.values().filter(|entry| entry.shared).count();
    let shared_bytes =
        plan.values()
            .filter(|entry| entry.shared)
            .try_fold(0u64, |total, entry| {
                require(
                    entry.size <= detach::MAX_FILE_BYTES,
                    "shared SDK file exceeds copy byte limit",
                )?;
                total
                    .checked_add(entry.size)
                    .ok_or_else(|| "shared SDK copy size overflow".to_owned())
            })?;
    require(
        shared_count <= 100_000 && shared_bytes <= 16 * 1024 * 1024 * 1024,
        "shared SDK copy exceeds count/total-byte limit",
    )?;
    ops.metadata(root)?;
    for (path, entry) in &plan {
        if matches!(entry.kind, Kind::Link(_)) {
            ops.link(path, entry, false, false)?;
        }
    }
    for (path, id) in &ancestors {
        ops.inspect(path, Some(id), true)?;
    }
    // Secure every directory before publishing fresh files. Shared source inodes
    // are read only: never chown/chmod/clear their ACL through any alias.
    for (path, entry) in &plan {
        if entry.kind == Kind::Directory {
            ops.inspect(path, Some(&entry.id), true)?;
        }
    }
    let shared: Vec<_> = plan
        .iter()
        .filter(|(_, entry)| entry.shared)
        .map(|(path, _)| path.clone())
        .collect();
    for path in shared {
        let entry = &plan[&path];
        let parent = plan
            .get(path.parent().ok_or("missing shared-file parent")?)
            .ok_or("unplanned shared-file parent")?;
        let id = ops.detach(&path, entry, parent)?;
        let entry = plan.get_mut(&path).ok_or("missing shared file")?;
        entry.id = id;
        entry.shared = false;
    }
    for (path, entry) in &plan {
        if matches!(entry.kind, Kind::Link(_)) {
            ops.link(path, entry, true, true)?;
        } else {
            ops.inspect(path, Some(&entry.id), true)?;
        }
    }
    require(
        plan_tree(root)? == plan,
        "SDK tree changed during preparation",
    )?;
    for (path, id) in &ancestors {
        ops.inspect(path, Some(id), false)?;
    }
    for (path, entry) in &plan {
        if matches!(entry.kind, Kind::Link(_)) {
            ops.link(path, entry, false, true)?;
        } else {
            ops.inspect(path, Some(&entry.id), false)?;
        }
    }
    ops.metadata(root)?;
    ops.select()?;
    Ok(plan.len())
}
fn guards(macos: bool, uid: u32, argc: usize) -> Result<()> {
    require(macos && uid == 0, "requires macOS root")?;
    require(argc == 1, "no arguments or SDK overrides permitted")
}
