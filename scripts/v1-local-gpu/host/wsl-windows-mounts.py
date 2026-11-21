"""Build and verify the dedicated WSL services' Windows mount masks."""
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys


WINDOWS_FILESYSTEMS = {"9p", "drvfs", "plan9", "virtio-plan9", "virtiofs", "fuse.drvfs", "fuse.virtiofs"}
WSL_SHARED_MOUNTS = {"/mnt/wsl", "/mnt/wslg"}


def mount_records(mountinfo):
    for line in mountinfo.splitlines():
        before, after = line.split(" - ", 1)
        fields = before.split()
        target = re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), fields[4])
        yield target, after.split()[0], fields[2]


def windows_mounts(mountinfo):
    return dict(sorted(
        (target, (filesystem, device))
        for target, filesystem, device in mount_records(mountinfo)
        if filesystem in WINDOWS_FILESYSTEMS
    ))


def masks(targets):
    directories, files = [], []
    for target in sorted(targets):
        if any(parent in directories for parent in map(str, PurePosixPath(target).parents)):
            continue
        (directories if os.path.isdir(target) else files).append(target)
    return directories, files


def policy(directories, files):
    lines = []
    for path in directories:
        if path == "/" or ":" in path:
            raise ValueError(f"Cannot express a read-only tmpfs mask for {path!r}")
        lines.append("TemporaryFileSystem=" + json.dumps(path.replace("%", "%%") + ":ro", ensure_ascii=False))
    for path in files:
        lines.append("InaccessiblePaths=" + json.dumps(path.replace("%", "%%"), ensure_ascii=False))
    return lines


def verify(targets, directories, files, socket, mountinfo):
    devices = {}
    for _, filesystem, device in mount_records(mountinfo):
        devices.setdefault(device, set()).add(filesystem)

    def filesystems(path):
        dev = os.stat(path).st_dev
        return devices.get(f"{os.major(dev)}:{os.minor(dev)}", set())

    for path in sorted(set(targets) | windows_mounts(mountinfo).keys()):
        if path in targets and os.access(path, os.W_OK):
            raise ValueError(f"Windows mount target is writable: {path!r}")
        try:
            visible_types = filesystems(path)
        except (FileNotFoundError, NotADirectoryError):
            continue
        if visible_types & WINDOWS_FILESYSTEMS:
            raise ValueError(f"Windows mount still visible instead of a mask: {path!r} (expected tmpfs or inaccessible)")

    for root in directories:
        if filesystems(root) != {"tmpfs"} or not os.statvfs(root).f_flag & os.ST_RDONLY:
            raise ValueError(f"Expected a read-only tmpfs at {root!r}")
        path = root
        # The SSH namespace may have only the selected socket and its parent directories.
        relative = socket[len(root) + 1:].split("/") if socket.startswith(root + "/") else []
        for component in relative:
            if not os.statvfs(path).f_flag & os.ST_RDONLY or set(os.listdir(path)) != {component}:
                raise ValueError(f"Unexpected entries in Windows mount mask {path!r}")
            path = os.path.join(path, component)
        if relative and not stat.S_ISSOCK(os.stat(path).st_mode):
            raise ValueError(f"Expected only the shared socket at {path!r}")
        if not relative and os.listdir(path):
            raise ValueError(f"Unexpected entries in Windows mount mask {path!r}")

    for path in files:
        if os.access(path, os.R_OK) or os.access(path, os.X_OK):
            raise ValueError(f"Windows file mount is accessible: {path!r}")


def trusted_driver_file(path):
    driver_root = Path("/usr/lib/wsl")
    resolved = path.resolve(strict=False)
    if driver_root not in resolved.parents or not resolved.is_file():
        raise ValueError(f"Expected a WSL driver file for CDI, got {path!s}")
    for part in (resolved, *resolved.parents):
        info = part.stat()
        if info.st_uid or info.st_mode & 0o022:
            raise ValueError(f"CDI driver file or parent is writable by non-root: {part!s}")
    return resolved


def stage_cdi(spec, masks, destination, installed_path):
    mask_paths = [Path(path) for path in masks]
    copies = {}
    edits = [spec.get("containerEdits", {}), *(device.get("containerEdits", {}) for device in spec["devices"])]
    for edit in edits:
        for mount in edit.get("mounts", []):
            path = Path(mount["hostPath"])
            resolved = path.resolve(strict=False)
            if not any(p.is_relative_to(mask) for mask in mask_paths for p in (path, resolved)):
                continue
            resolved = trusted_driver_file(path)
            if resolved not in copies:
                name = str(len(copies))
                shutil.copyfile(resolved, destination / name)
                os.chmod(destination / name, 0o555)
                copies[resolved] = os.path.join(installed_path, name)
            mount["hostPath"] = copies[resolved]


def mountinfo():
    with open("/proc/self/mountinfo", encoding="utf-8", errors="surrogateescape") as handle:
        return handle.read()


def main(args):
    command, *rest = args
    if command == "plan":
        info = mountinfo()
        targets = windows_mounts(info)
        # Retain isolation of the WSL inter-distro mounts independently of Windows drive detection.
        shared = {target for target, _, _ in mount_records(info) if target in WSL_SHARED_MOUNTS}
        directories, files = masks(targets.keys() | shared)
        print(json.dumps({"targets": targets, "directories": directories, "files": files}))
        return
    plan = json.loads(rest[0])
    directories, files = plan["directories"], plan["files"]
    if command == "policy":
        for line in policy(directories, files):
            print(line)
    elif command == "paths":
        for path in plan[rest[1]]:
            sys.stdout.buffer.write(os.fsencode(path) + b"\0")
    elif command == "verify":
        verify(plan["targets"], directories, files, rest[1], mountinfo())
    elif command == "stage-cdi":
        spec = json.load(sys.stdin)
        destination = Path(rest[1])
        stage_cdi(spec, directories + files, destination, rest[2])
        output = destination / "nvidia.json"
        output.write_text(json.dumps(spec) + "\n", encoding="utf-8")
        output.chmod(stat.S_IRUSR | stat.S_IRGRP | stat.S_IROTH)
    else:
        raise ValueError(f"Unknown command: {command!r}")


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except (OSError, ValueError) as exc:
        raise SystemExit(str(exc))
