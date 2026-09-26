#!/usr/bin/python3
"""Prepare only the pinned CI SDK; never run this against a developer workstation.

Workflow placement: immediately after pinned checkout, before toolchain setup or
any other repository code. Run with sudo /usr/bin/python3 -I, with no arguments.
No SDK path override is intentionally provided. Tests call pure planning helpers.
Directory-relative, no-follow descriptors keep mutations on inspected inodes.
Symlinks are not traversed or chmod'ed; only their own ownership is normalized.
"""
import ctypes
import errno
import json
import os
from pathlib import Path
import plistlib
import stat
import subprocess
import sys

DEVELOPER = Path('/Applications/Xcode_26.5.app/Contents/Developer')
SDKS = DEVELOPER / 'Platforms/MacOSX.platform/Developer/SDKs'
ENV = {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin'}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(args, developer=False):
    env = dict(ENV)
    if developer:
        env['DEVELOPER_DIR'] = str(DEVELOPER)
    return subprocess.check_output(args, env=env, cwd='/', text=True, timeout=30).strip()


def discover(developer):
    values = [command(['/usr/bin/xcrun', '--no-cache', '--sdk', 'macosx26.5', flag],
                      developer) for flag in
              ('--show-sdk-path', '--show-sdk-version', '--show-sdk-build-version')]
    require(values[1:] == ['26.5', '25F70'], 'xcrun SDK version/build mismatch')
    return sdk_path(values[0]), values


def sdk_path(text):
    path = Path(text)
    require(path.is_absolute() and not any(c in text for c in '\r\n\0'),
            'invalid SDK path')
    require(path.parent == SDKS and path.name in ('MacOSX.sdk', 'MacOSX26.5.sdk'),
            'SDK is outside the pinned installation')
    resolved = path.resolve(strict=True)
    require(resolved.parent == SDKS and resolved.name in ('MacOSX.sdk', 'MacOSX26.5.sdk'),
            'canonical SDK is outside the pinned installation')
    return resolved


def metadata(settings, system):
    require(isinstance(settings, dict) and isinstance(system, dict), 'invalid SDK metadata')
    require(settings.get('CanonicalName') == 'macosx26.5'
            and settings.get('Version') == '26.5'
            and system.get('ProductName') == 'macOS'
            and system.get('ProductVersion') == '26.5'
            and system.get('ProductBuildVersion') == '25F70', 'SDK metadata mismatch')


def identity(info):
    return (info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode))


def entry_kind(info):
    require(not info.st_mode & 0o7000, 'special permission bits on SDK entry')
    if stat.S_ISDIR(info.st_mode):
        return 'directory'
    if stat.S_ISREG(info.st_mode):
        require(info.st_nlink == 1, 'hardlinked SDK file')
        return 'file'
    require(stat.S_ISLNK(info.st_mode), 'unsupported SDK entry type')
    return 'symlink'


def check_link(path, root):
    target = os.readlink(path)
    require(target and not os.path.isabs(target) and not any(c in target for c in '\0\r\n'),
            'unsafe SDK symlink target: ' + str(path))
    # Reject even a transient lexical escape, as well as chained escapes/loops.
    cursor = path.parent
    for part in Path(target).parts:
        cursor = cursor.parent if part == '..' else cursor / part
        require(cursor == root or root in cursor.parents, 'SDK symlink escapes tree')
    resolved = path.resolve(strict=True)
    require(resolved == root or root in resolved.parents, 'SDK symlink escapes tree')
    require(resolved.is_dir() or resolved.is_file(), 'unsupported symlink destination')
    return target


def plan_tree(root):
    """Read-only planning, including every symlink and rejecting special files."""
    plan = {}
    def visit(path):
        info = path.lstat()
        kind = entry_kind(info)
        target = check_link(path, root) if kind == 'symlink' else None
        plan[path] = (identity(info), kind, target)
        if kind == 'directory':
            for name in sorted(os.listdir(path)):
                visit(path / name)
    visit(root)
    return plan


class ACL:
    """Darwin extended ACL APIs. Mutations use file descriptors, never paths."""
    def __init__(self):
        self.lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
        for name, args, result in (
            ('acl_get_fd_np', [ctypes.c_int, ctypes.c_int], ctypes.c_void_p),
            ('acl_get_link_np', [ctypes.c_char_p, ctypes.c_int], ctypes.c_void_p),
            ('acl_get_entry', [ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_void_p)], ctypes.c_int),
            ('acl_init', [ctypes.c_int], ctypes.c_void_p),
            ('acl_set_fd_np', [ctypes.c_int, ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
            ('acl_free', [ctypes.c_void_p], ctypes.c_int),
        ):
            fn = getattr(self.lib, name)
            fn.argtypes, fn.restype = args, result

    def present(self, fd=None, link=None):
        ctypes.set_errno(0)
        acl = (self.lib.acl_get_link_np(os.fsencode(link), 0x100) if link is not None
               else self.lib.acl_get_fd_np(fd, 0x100))
        if not acl:
            require(ctypes.get_errno() == errno.ENOENT, 'cannot read SDK ACL')
            return False
        try:
            entry = ctypes.c_void_p()
            result = self.lib.acl_get_entry(acl, 0, ctypes.byref(entry))
            if result == -1:
                require(ctypes.get_errno() == errno.EINVAL, 'cannot enumerate SDK ACL')
                return False
            require(result == 0, 'unexpected ACL result')
            return True
        finally:
            self.lib.acl_free(acl)

    def clear(self, fd):
        acl = self.lib.acl_init(0)
        require(bool(acl), 'cannot allocate empty ACL')
        try:
            require(self.lib.acl_set_fd_np(fd, acl, 0x100) == 0, 'cannot clear SDK ACL')
        finally:
            self.lib.acl_free(acl)


def open_node(path):
    """Open each absolute component without following any symlink."""
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.parts[1:]:
            next_fd = os.open(part, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return fd
    except BaseException:
        os.close(fd)
        raise


def trusted(path, info):
    mode = stat.S_IMODE(info.st_mode)
    return info.st_uid == 0 and not mode & 0o002 and (
        not mode & 0o020 or (path == Path('/Applications') and info.st_gid == 80 and mode == 0o775))


def read_metadata(root):
    values = []
    for relative, parser in [('SDKSettings.json', json.loads),
                             ('System/Library/CoreServices/SystemVersion.plist', plistlib.loads)]:
        fd = open_node(root / relative)
        with os.fdopen(fd, 'rb') as stream:
            require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), 'metadata is not a file')
            data = stream.read(65537)
        require(len(data) <= 65536, 'oversized SDK metadata')
        values.append(parser(data))
    metadata(*values)
    print(json.dumps({'SDKSettings': values[0], 'SystemVersion': values[1]}, sort_keys=True))


def inspect_node(path, acl, expected=None, mutate=False):
    evidence = len(path.parts) <= len(SDKS.parts) + 1 or path.name in ('SDKSettings.json', 'SystemVersion.plist')
    fd = open_node(path)
    try:
        before = os.fstat(fd)
        entry_kind(before)
        if expected is not None:
            require(identity(before) == expected, 'SDK inode changed: ' + str(path))
        has_acl = acl.present(fd=fd)
        if mutate and evidence:
            print(json.dumps({'before': str(path), 'uid': before.st_uid, 'gid': before.st_gid,
                              'mode': oct(stat.S_IMODE(before.st_mode)), 'acl': has_acl}))
        if mutate:
            os.fchown(fd, 0, 0)
            acl.clear(fd)
            os.fchmod(fd, stat.S_IMODE(before.st_mode) & ~0o022)
        after = os.fstat(fd)
        require(trusted(path, after) and not acl.present(fd=fd), 'unsafe SDK node: ' + str(path))
        if mutate and evidence:
            print(json.dumps({'after': str(path), 'uid': after.st_uid, 'gid': after.st_gid,
                              'mode': oct(stat.S_IMODE(after.st_mode)), 'acl': False}))
    finally:
        os.close(fd)


def prepare():
    require(sys.platform == 'darwin' and os.geteuid() == 0, 'requires macOS root')
    require(len(sys.argv) == 1, 'no arguments or SDK overrides permitted')
    acl = ACL()
    # Root and /Applications are policy checks only; never normalize either.
    for path in (Path('/'), Path('/Applications')):
        inspect_node(path, acl)
    root, discovery = discover(True)
    print(json.dumps({'discovery_before': discovery}))
    ancestors = list(reversed(root.parents))[2:]
    for path in ancestors:
        require(stat.S_ISDIR(path.lstat().st_mode), 'symlink/non-directory SDK ancestor')
    identities = {path: identity(path.lstat()) for path in ancestors}
    plan = plan_tree(root)
    read_metadata(root)  # All pinned metadata must pass before the first mutation.
    for path, (_, kind, _) in plan.items():
        if kind == 'symlink':
            require(not acl.present(link=path), 'SDK symlink has an ACL')
    # Protect top down. Retained descriptors prevent mutation through final links;
    # inode checks and full replanning detect replacement during preparation.
    for path in ancestors:
        inspect_node(path, acl, identities[path], mutate=True)
    for path, (expected, kind, target) in plan.items():
        if kind != 'symlink':
            inspect_node(path, acl, expected, mutate=True)
            continue
        parent_fd = open_node(path.parent)
        try:
            before = os.stat(path.name, dir_fd=parent_fd, follow_symlinks=False)
            require(identity(before) == expected and os.readlink(path.name, dir_fd=parent_fd) == target,
                    'SDK symlink changed')
            os.chown(path.name, 0, 0, dir_fd=parent_fd, follow_symlinks=False)
            after = os.stat(path.name, dir_fd=parent_fd, follow_symlinks=False)
            require(identity(after) == expected and after.st_uid == 0, 'SDK symlink changed')
            require(not acl.present(link=path), 'SDK symlink ACL changed')
        finally:
            os.close(parent_fd)
    require(plan_tree(root) == plan, 'SDK tree changed during preparation')
    for path in ancestors:
        inspect_node(path, acl, identities[path])
    for path, (expected, kind, _) in plan.items():
        if kind == 'symlink':
            require(path.lstat().st_uid == 0 and not acl.present(link=path), 'unsafe SDK symlink')
        else:
            inspect_node(path, acl, expected)
    read_metadata(root)
    # The production validator clears DEVELOPER_DIR, so change the system selector
    # only after independent verification, then repeat discovery without overrides.
    command(['/usr/bin/xcode-select', '--switch', str(DEVELOPER)])
    final_root, final_discovery = discover(False)
    require(final_root == root, 'selected SDK changed')
    print(json.dumps({'discovery_after': final_discovery, 'verified_entries': len(plan)}))


if __name__ == '__main__':
    try:
        prepare()
    except Exception as error:
        sys.exit('SDK preparation failed: ' + str(error))
