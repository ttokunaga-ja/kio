//! Reject renderer bind sources that expose the host control plane through
//! filesystem aliases or recursive submounts. This is a mount-namespace snapshot:
//! privileged concurrent mount mutation is outside the renderer threat model.
//! Call immediately before constructing bwrap's binds, with actual canonical
//! source paths, never the synthetic destinations inside the sandbox.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::OpenOptionsExt,
        },
    },
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_ROWS: usize = 4096;
const MAX_FIELDS: usize = 128;
const MAX_FIELD_BYTES: usize = 16 * 1024;
const MAX_FDINFO_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Device(u32, u32);

#[derive(Debug)]
struct Mount {
    id: u64,
    parent: u64,
    device: Device,
    root: PathBuf,
    point: PathBuf,
    kind: Vec<u8>,
}

#[derive(Debug)]
struct Region {
    device: Device,
    path: PathBuf,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "renderer bind source exposes a host control-plane filesystem",
    )
}

fn normalized_absolute(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes == b"/"
        || (bytes.starts_with(b"/")
            && bytes[1..].split(|byte| *byte == b'/').all(|part| {
                !part.is_empty() && part != b"." && part != b".." && !part.contains(&0)
            }))
}

fn number(bytes: &[u8]) -> io::Result<u64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid("invalid mountinfo numeric field"));
    }
    std::str::from_utf8(bytes)
        .map_err(|_| invalid("invalid mountinfo number"))?
        .parse()
        .map_err(|_| invalid("overflowing mountinfo number"))
}

fn decode_path(bytes: &[u8]) -> io::Result<PathBuf> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let escape = bytes
                .get(index + 1..index + 4)
                .ok_or_else(|| invalid("truncated mountinfo escape"))?;
            // These are the four escapes emitted by the kernel's seq_path().
            decoded.push(match escape {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(invalid("invalid mountinfo path escape")),
            });
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let path = PathBuf::from(OsString::from_vec(decoded));
    if !normalized_absolute(&path) {
        return Err(invalid("mountinfo path is not normalized and absolute"));
    }
    Ok(path)
}

fn parse_table(bytes: &[u8]) -> io::Result<Vec<Mount>> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES || !bytes.ends_with(b"\n") {
        return Err(invalid("missing, oversized, or truncated mountinfo table"));
    }
    let mut mounts = Vec::new();
    let mut ids = BTreeSet::new();
    for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        if mounts.len() == MAX_ROWS {
            return Err(invalid("too many mountinfo rows"));
        }
        let fields: Vec<_> = line
            .split(|byte| *byte == b' ')
            .take(MAX_FIELDS + 1)
            .collect();
        if fields.len() < 10
            || fields.len() > MAX_FIELDS
            || fields.iter().any(|field| {
                field.is_empty()
                    || field.len() > MAX_FIELD_BYTES
                    || field.iter().any(|byte| byte.is_ascii_control())
            })
        {
            return Err(invalid("invalid or oversized mountinfo fields"));
        }
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or_else(|| invalid("missing mountinfo separator"))?;
        if separator < 6 || separator + 4 != fields.len() {
            return Err(invalid("invalid mountinfo record layout"));
        }
        for options in [fields[5], fields[separator + 3]] {
            let mut parts = options.split(|byte| *byte == b',');
            if !matches!(parts.next(), Some(b"rw" | b"ro")) || parts.any(|part| part.is_empty()) {
                return Err(invalid("invalid mountinfo options"));
            }
        }
        for optional in &fields[6..separator] {
            let tag = optional
                .split(|byte| *byte == b':')
                .next()
                .unwrap_or_default();
            if tag.is_empty()
                || !tag
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(byte))
            {
                return Err(invalid("invalid optional mountinfo tag"));
            }
            if matches!(tag, b"shared" | b"master" | b"propagate_from") {
                let value = optional
                    .get(tag.len() + 1..)
                    .ok_or_else(|| invalid("missing optional mountinfo number"))?;
                if number(value)? == 0 {
                    return Err(invalid("zero optional mountinfo number"));
                }
            }
        }
        let id = number(fields[0])?;
        let parent = number(fields[1])?;
        let device: Vec<_> = fields[2].split(|byte| *byte == b':').collect();
        if id == 0 || parent == 0 || device.len() != 2 || !ids.insert(id) {
            return Err(invalid("invalid or duplicate mountinfo identity"));
        }
        let major = u32::try_from(number(device[0])?)
            .map_err(|_| invalid("overflowing mountinfo device"))?;
        let minor = u32::try_from(number(device[1])?)
            .map_err(|_| invalid("overflowing mountinfo device"))?;
        let root = decode_path(fields[3])?;
        let point = decode_path(fields[4])?;
        let kind = fields[separator + 1];
        if !kind
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
        {
            return Err(invalid("invalid mountinfo filesystem type"));
        }
        mounts.push(Mount {
            id,
            parent,
            device: Device(major, minor),
            root,
            point,
            kind: kind.to_vec(),
        });
    }
    if !mounts.iter().any(|mount| mount.point == Path::new("/")) {
        return Err(invalid("missing namespace root mount"));
    }
    let by_id: BTreeMap<_, _> = mounts.iter().map(|mount| (mount.id, mount)).collect();
    for mount in &mounts {
        match by_id.get(&mount.parent) {
            Some(parent) if mount.point.starts_with(&parent.point) && mount.id != mount.parent => {}
            Some(_) if mount.point == Path::new("/") && mount.id == mount.parent => {}
            None if mount.point == Path::new("/") => {}
            _ => return Err(invalid("invalid or missing mountinfo parent")),
        }
    }
    let mut checked = BTreeSet::new();
    for mount in &mounts {
        let mut visited = BTreeSet::new();
        let mut current = mount;
        while current.parent != current.id && !checked.contains(&current.id) {
            if !visited.insert(current.id) {
                return Err(invalid("cyclic mountinfo parents"));
            }
            match by_id.get(&current.parent) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        checked.extend(visited);
    }
    Ok(mounts)
}

fn parse_mount_id(bytes: &[u8]) -> io::Result<u64> {
    if bytes.is_empty() || bytes.len() > MAX_FDINFO_BYTES || !bytes.ends_with(b"\n") {
        return Err(invalid("missing, oversized, or truncated fdinfo"));
    }
    let mut id = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"mnt_id:") {
            if id.is_some() {
                return Err(invalid("duplicate fdinfo mount ID"));
            }
            let value = value.trim_ascii();
            let value = number(value)?;
            if value == 0 {
                return Err(invalid("zero fdinfo mount ID"));
            }
            id = Some(value);
        }
    }
    id.ok_or_else(|| invalid("fdinfo has no mount ID"))
}

fn kernel_mount_id(path: &Path) -> io::Result<u64> {
    // O_PATH from Linux asm-generic/fcntl.h. This opens sockets, directories,
    // and unreadable regular files without reading contents or causing device
    // I/O. The descriptor remains open until fdinfo has been read, so its
    // numeric FD cannot be reused by another thread during this lookup.
    const O_PATH: i32 = 0o10000000;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_PATH)
        .open(path)?;
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?
        .take((MAX_FDINFO_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    parse_mount_id(&bytes)
}

fn resolved_mount<'a>(
    mounts: &'a [Mount],
    path: &Path,
    resolve: &mut impl FnMut(&Path) -> io::Result<u64>,
) -> io::Result<&'a Mount> {
    let id = resolve(path)?;
    let mount = mounts
        .iter()
        .find(|mount| mount.id == id)
        .ok_or_else(|| invalid("kernel mount ID is absent from mountinfo snapshot"))?;
    if !path.starts_with(&mount.point) {
        return Err(invalid("kernel mount ID does not cover requested path"));
    }
    Ok(mount)
}

fn region(mount: &Mount, path: &Path) -> io::Result<Region> {
    let relative = path
        .strip_prefix(&mount.point)
        .map_err(|_| invalid("path is outside its mount"))?;
    Ok(Region {
        device: mount.device,
        path: mount.root.join(relative),
    })
}

fn included_regions(
    mounts: &[Mount],
    paths: &[PathBuf],
    reject_special: bool,
    resolve: &mut impl FnMut(&Path) -> io::Result<u64>,
) -> io::Result<Vec<Region>> {
    let mut points = BTreeSet::new();
    for path in paths {
        if !normalized_absolute(path) {
            return Err(invalid("bind source is not normalized and absolute"));
        }
        points.insert(path.as_path());
    }
    for mount in mounts {
        if paths.iter().any(|path| mount.point.starts_with(path)) {
            points.insert(mount.point.as_path());
        }
    }
    let mut regions = Vec::new();
    for path in points {
        // Query the kernel even for descendant mountpoints. A hidden old child
        // may resolve through a different covering mount; the positive mount ID
        // selects that visible backing path. Missing/inaccessible paths fail
        // closed rather than assuming the old mount is hidden or harmless.
        let mount = resolved_mount(mounts, path, resolve)?;
        if reject_special
            && matches!(
                mount.kind.as_slice(),
                b"proc" | b"sysfs" | b"cgroup" | b"cgroup2"
            )
        {
            return Err(denied());
        }
        regions.push(region(mount, path)?);
    }
    Ok(regions)
}

fn validate_table(
    mounts: &[Mount],
    sources: &[PathBuf],
    protected: &[PathBuf],
    mut resolve: impl FnMut(&Path) -> io::Result<u64>,
) -> io::Result<()> {
    // At most one descriptor lookup for each source, protected root, or unique
    // relevant mountpoint. No descriptor is retained across separate lookups.
    let mut cache = BTreeMap::new();
    let mut cached_resolve = |path: &Path| -> io::Result<u64> {
        if let Some(id) = cache.get(path) {
            return Ok(*id);
        }
        let id = resolve(path)?;
        cache.insert(path.to_path_buf(), id);
        Ok(id)
    };
    let protected = included_regions(mounts, protected, false, &mut cached_resolve)?;
    let included = included_regions(mounts, sources, true, &mut cached_resolve)?;
    for source in included {
        if protected.iter().any(|target| {
            source.device == target.device
                && (source.path.starts_with(&target.path) || target.path.starts_with(&source.path))
        }) {
            return Err(denied());
        }
    }
    Ok(())
}

/// Validate actual canonical bind sources before recursive bubblewrap binds.
/// Both mountinfo failures and ambiguous mappings deny the renderer launch.
/// No privileged operations or host mount changes are performed.
pub(super) fn validate_sources(sources: &[PathBuf]) -> io::Result<()> {
    if sources.len() > MAX_ROWS {
        return Err(invalid("too many renderer bind sources"));
    }
    for source in sources {
        if !normalized_absolute(source)
            || fs::canonicalize(source)?.as_os_str() != source.as_os_str()
        {
            return Err(invalid("renderer bind source must already be canonical"));
        }
    }
    let protected = ["/run", "/sys", "/proc"]
        .into_iter()
        .map(fs::canonicalize)
        .collect::<io::Result<Vec<_>>>()?;
    let mut bytes = Vec::new();
    File::open("/proc/self/mountinfo")?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    validate_table(&parse_table(&bytes)?, sources, &protected, kernel_mount_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "1 99 8:1 / / rw - ext4 /dev/root rw\n";
    const CONTROL: &str = "2 1 0:2 / /run rw - tmpfs tmpfs rw\n3 1 0:3 / /proc rw - proc proc rw\n4 1 0:4 / /sys rw - sysfs sysfs rw\n5 2 0:5 / /run/user/1000 rw - tmpfs tmpfs rw\n";

    fn fixture_resolve(mounts: &[Mount], path: &Path) -> io::Result<u64> {
        let candidates: Vec<_> = mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.point))
            .collect();
        let depth = candidates
            .iter()
            .map(|mount| mount.point.components().count())
            .max()
            .ok_or_else(|| invalid("fixture lacks mount"))?;
        let mut matching = candidates
            .into_iter()
            .filter(|mount| mount.point.components().count() == depth);
        let id = matching.next().unwrap().id;
        if matching.next().is_some() {
            return Err(invalid("fixture needs explicit kernel mount ID"));
        }
        Ok(id)
    }

    fn fixture_validate(
        mounts: &[Mount],
        sources: &[PathBuf],
        protected: &[PathBuf],
    ) -> io::Result<()> {
        validate_table(mounts, sources, protected, |path| {
            fixture_resolve(mounts, path)
        })
    }

    fn check(extra: &str, paths: &[&str]) -> io::Result<()> {
        let mounts = parse_table(format!("{ROOT}{CONTROL}{extra}").as_bytes())?;
        fixture_validate(
            &mounts,
            &paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
            &["/run".into(), "/sys".into(), "/proc".into()],
        )
    }

    #[test]
    fn rejects_direct_and_aliased_control_paths() {
        for source in ["/run", "/run/user/1000/bus", "/sys", "/proc/self", "/"] {
            assert!(check("", &[source]).is_err(), "{source}");
        }
        for (device, root) in [("0:2", "/"), ("0:5", "/"), ("0:5", "/bus")] {
            let extra = format!("6 1 {device} {root} /alias rw - tmpfs tmpfs rw\n");
            assert!(check(&extra, &["/alias"]).is_err());
        }
    }

    #[test]
    fn recursive_binds_include_nested_aliases_and_special_filesystems() {
        assert!(
            check(
                "6 1 0:5 /bus /opt/runtime/socket rw - tmpfs tmpfs rw\n",
                &["/opt/runtime"]
            )
            .is_err()
        );
        for kind in ["proc", "sysfs", "cgroup", "cgroup2"] {
            let extra = format!("6 1 0:9 / /opt/runtime/hidden rw - {kind} none rw\n");
            assert!(check(&extra, &["/opt/runtime"]).is_err());
        }
    }

    #[test]
    fn permits_unrelated_backing_paths_and_wsl_mounts() {
        assert!(check("", &["/usr", "/usr/bin/python3", "/run-other"]).is_ok());
        // A rootfs directory called /run is distinct from the live tmpfs /run.
        assert!(
            check(
                "6 1 8:1 /run /old-run rw - ext4 /dev/root rw\n",
                &["/old-run"]
            )
            .is_ok()
        );
        assert!(check("6 1 0:6 / /usr/lib/wsl rw - 9p none rw\n", &["/usr"]).is_ok());
    }

    #[test]
    fn shared_root_filesystem_uses_component_overlap() {
        let mounts = parse_table(
            format!("{ROOT}6 1 8:1 /run/user/1000/bus /alias rw - ext4 /dev/root rw\n").as_bytes(),
        )
        .unwrap();
        let protected = ["/run".into(), "/sys".into(), "/proc".into()];
        assert!(fixture_validate(&mounts, &["/alias".into()], &protected).is_err());
        assert!(fixture_validate(&mounts, &["/usr".into(), "/runner".into()], &protected).is_ok());
    }

    #[test]
    fn decodes_kernel_escapes_and_preserves_non_utf8() {
        assert_eq!(
            decode_path(br"/a\040b\011c\012d\134e")
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"/a b\tc\nd\\e"
        );
        assert_eq!(
            decode_path(b"/a\xff").unwrap().as_os_str().as_bytes(),
            b"/a\xff"
        );
        assert!(
            check(
                "6 1 0:5 /bus /opt/with\\040space/socket rw - tmpfs tmpfs rw\n",
                &["/opt/with space"]
            )
            .is_err()
        );
        assert!(
            check(
                "6 1 8:1 /safe\\040space /opt/with\\040space rw - ext4 /dev/root rw\n",
                &["/opt/with space"]
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_malformed_missing_and_oversized_tables() {
        for input in [
            "",
            "1 99 8:1 / / rw - ext4 none rw",
            "1 99 8:1 / / rw ext4 none rw\n",
            "1 99 x:y / / rw - ext4 none rw\n",
            "1 99 8:1 / /bad/../path rw - ext4 none rw\n",
            "1 99 8:1 / /bad\\041escape rw - ext4 none rw\n",
            "1 99 8:1 / / rw - ext4 none rw extra\n",
            "1 99 8:1 / / rw - ext4 none rw\n\n",
            "1 99 8:1 / /usr rw - ext4 none rw\n",
        ] {
            assert!(parse_table(input.as_bytes()).is_err(), "{input:?}");
        }
        assert!(parse_table(&vec![b'x'; MAX_BYTES + 1]).is_err());
        assert!(
            parse_table(
                format!(
                    "{ROOT}2 1 0:2 / /x {} - tmpfs none rw\n",
                    "a".repeat(MAX_FIELD_BYTES + 1)
                )
                .as_bytes()
            )
            .is_err()
        );
        assert!(
            parse_table(
                format!(
                    "{ROOT}{}",
                    (2..=MAX_ROWS + 1)
                        .map(|id| format!("{id} 1 0:2 / /x{id} rw - tmpfs none rw\n"))
                        .collect::<String>()
                )
                .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_ambiguous_stacks_ids_and_missing_parents() {
        for extra in [
            "6 1 0:7 / /run rw - tmpfs none rw\n",
            "2 1 0:7 / /other rw - tmpfs none rw\n",
            "6 999 0:7 / /other rw - tmpfs none rw\n",
            "6 5 0:7 / /other rw - tmpfs none rw\n",
        ] {
            assert!(check(extra, &["/usr"]).is_err());
        }
    }

    #[test]
    fn hidden_old_submount_does_not_replace_visible_parent_mapping() {
        // The /opt bind covers an older child belonging to mount 1. It must
        // still expose /run's backing storage, regardless of mountinfo order.
        let extra = "6 1 0:2 / /opt rw - tmpfs none rw\n7 1 8:1 /safe /opt/old rw - ext4 none rw\n";
        let mounts = parse_table(format!("{ROOT}{CONTROL}{extra}").as_bytes()).unwrap();
        let result = validate_table(
            &mounts,
            &["/opt/old".into()],
            &["/run".into(), "/sys".into(), "/proc".into()],
            |path| {
                if path == Path::new("/opt/old") {
                    Ok(6)
                } else {
                    fixture_resolve(&mounts, path)
                }
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn source_relative_path_is_applied_to_bind_root() {
        let mounts =
            parse_table(format!("{ROOT}6 1 8:1 / /alias rw - ext4 none rw\n").as_bytes()).unwrap();
        let protected = ["/run".into(), "/sys".into(), "/proc".into()];
        assert!(
            fixture_validate(&mounts, &["/alias/run/user/1000/bus".into()], &protected).is_err()
        );
        assert!(fixture_validate(&mounts, &["/alias/usr".into()], &protected).is_ok());
    }

    #[test]
    fn rejects_invalid_options_and_accepts_propagation_tags() {
        assert!(parse_table(b"1 99 8:1 / / rw shared:1 master:2 - ext4 none rw\n").is_ok());
        for record in [
            "1 99 8:1 / / bad - ext4 none rw\n",
            "1 99 8:1 / / rw shared:bad - ext4 none rw\n",
            "1 99 8:1 / / rw shared - ext4 none rw\n",
            "1 99 8:1 / / rw - ext4 none rw,,nodev\n",
            "1 2 8:1 / / rw - ext4 none rw\n2 1 0:2 / /child rw - tmpfs none rw\n",
        ] {
            assert!(parse_table(record.as_bytes()).is_err());
        }
    }

    #[test]
    fn rejects_noncanonical_source_spelling() {
        for source in [
            "relative",
            "/usr/../run",
            "/usr/./bin",
            "/usr//bin",
            "/usr/",
        ] {
            assert!(check("", &[source]).is_err());
        }
    }
    #[test]
    fn kernel_resolver_selects_same_point_stack_not_row_order() {
        let extra = "6 2 0:6 / /run rw - tmpfs none rw\n7 1 0:6 /bus /alias rw - tmpfs none rw\n";
        let mounts = parse_table(format!("{ROOT}{CONTROL}{extra}").as_bytes()).unwrap();
        let protected = ["/run".into(), "/sys".into(), "/proc".into()];
        for reversed in [false, true] {
            let mut order: Vec<_> = (0..mounts.len()).collect();
            if reversed {
                order.reverse();
            }
            let text = order
                .iter()
                .map(|&index| {
                    let m = &mounts[index];
                    format!(
                        "{} {} {}:{} {} {} rw - {} none rw\n",
                        m.id,
                        m.parent,
                        m.device.0,
                        m.device.1,
                        m.root.display(),
                        m.point.display(),
                        String::from_utf8_lossy(&m.kind)
                    )
                })
                .collect::<String>();
            let parsed = parse_table(text.as_bytes()).unwrap();
            let result = validate_table(&parsed, &["/alias".into()], &protected, |path| {
                if path.starts_with("/run") {
                    Ok(6)
                } else {
                    fixture_resolve(&parsed, path)
                }
            });
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn hidden_old_child_requires_positive_visible_cover() {
        let extra =
            "6 1 0:6 / /opt rw - tmpfs none rw\n7 1 0:5 /bus /opt/hidden rw - tmpfs none rw\n";
        let mounts = parse_table(format!("{ROOT}{CONTROL}{extra}").as_bytes()).unwrap();
        let protected = ["/run".into(), "/sys".into(), "/proc".into()];
        assert!(
            validate_table(&mounts, &["/opt".into()], &protected, |path| {
                if path.starts_with("/opt") {
                    Ok(6)
                } else {
                    fixture_resolve(&mounts, path)
                }
            })
            .is_ok()
        );
        assert!(
            validate_table(&mounts, &["/opt".into()], &protected, |path| {
                if path == Path::new("/opt/hidden") {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                } else {
                    fixture_resolve(&mounts, path)
                }
            })
            .is_err()
        );
    }

    #[test]
    fn unrelated_stacks_do_not_need_resolution() {
        let extra =
            "6 1 0:6 / /Docker/host rw - 9p none rw\n7 6 0:7 / /Docker/host rw - 9p none rw\n";
        assert!(check(extra, &["/usr"]).is_ok());
    }

    #[test]
    fn kernel_id_must_exist_cover_path_and_be_readable() {
        let mounts = parse_table(format!("{ROOT}{CONTROL}").as_bytes()).unwrap();
        let protected = ["/run".into(), "/sys".into(), "/proc".into()];
        for id in [999, 2] {
            assert!(
                validate_table(&mounts, &["/usr".into()], &protected, |path| {
                    if path == Path::new("/usr") {
                        Ok(id)
                    } else {
                        fixture_resolve(&mounts, path)
                    }
                })
                .is_err()
            );
        }
        assert!(
            validate_table(&mounts, &["/usr".into()], &protected, |_| Err(
                io::Error::from(io::ErrorKind::PermissionDenied)
            ))
            .is_err()
        );
    }

    #[test]
    fn fdinfo_mount_id_is_bounded_and_unambiguous() {
        assert_eq!(
            parse_mount_id(b"pos:\t0\nflags:\t012000000\nmnt_id:\t42\nino:\t19\n").unwrap(),
            42
        );
        for bytes in [
            b"".as_slice(),
            b"mnt_id:\t0\n",
            b"mnt_id:\tbad\n",
            b"mnt_id:\t1",
            b"mnt_id:\t1\nmnt_id:\t2\n",
            b"pos:\t0\n",
        ] {
            assert!(parse_mount_id(bytes).is_err());
        }
        assert!(parse_mount_id(&vec![b' '; MAX_FDINFO_BYTES + 1]).is_err());
    }
}
