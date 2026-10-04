"""Run the embedded preflight with mocked OS operations, never shell setup."""

from contextlib import ExitStack, redirect_stderr, redirect_stdout
from io import StringIO
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SCRIPT = Path(os.environ.get(
    "KIO_CONFINEMENT_SCRIPT", Path(__file__).with_name("prepare-linux-confinement.sh")))
SOURCE = SCRIPT.read_text().split("python3 - <<'PYTHON'\n", 1)[1].split("\nPYTHON\n", 1)[0]
PING = ["busctl", "--user", "--timeout=10", "call", "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1", "org.freedesktop.DBus.Peer", "Ping"]


class PreflightTests(unittest.TestCase):
    def run_preflight(self, *, ping_failure=False, resource_failure=False,
                      resource_timeout=False, cleanup_failure=False,
                      runtime_mode=0o700, initial_env=""):
        calls = []
        unit = None
        original_stat, original_lstat = Path.stat, Path.lstat
        original_read, original_exists = Path.read_text, Path.exists
        original_resolve, original_open = Path.resolve, Path.open
        runtime = Path("/run/user/1001")
        capture = StringIO()

        def path_stat(path, *args, **kwargs):
            if path == runtime:
                return SimpleNamespace(st_mode=stat.S_IFDIR | runtime_mode, st_uid=1001)
            return original_stat(path, *args, **kwargs)

        def path_lstat(path, *args, **kwargs):
            if path == runtime / "bus":
                return SimpleNamespace(st_mode=stat.S_IFSOCK | 0o600, st_uid=1001)
            return original_lstat(path, *args, **kwargs)

        def path_resolve(path, *args, **kwargs):
            if path == runtime or str(path).startswith("/sys/fs/cgroup/"):
                return path
            return original_resolve(path, *args, **kwargs)

        def path_read(path, *args, **kwargs):
            if path == Path("/proc/self/cgroup"):
                return f"0::/user.slice/user-1001.slice/user@1001.service/app.slice/{unit}\n"
            if str(path).startswith("/sys/fs/cgroup/"):
                values = {"cgroup.controllers": "cpu memory pids",
                          "cgroup.subtree_control": "cpu memory pids",
                          "memory.max": "0" if resource_failure else "67108864",
                          "memory.swap.max": "0", "pids.max": "32",
                          "memory.oom.group": "1", "cpu.max": "100000 100000"}
                return values[path.name] + "\n"
            return original_read(path, *args, **kwargs)

        def path_exists(path):
            if str(path).startswith("/sys/fs/cgroup/"):
                return False  # The scope was collected after its child exited.
            return original_exists(path)

        def path_open(path, mode="r", *args, **kwargs):
            if path.name == "github-env" and mode == "a":
                self.assertIn("Verified resource controls and termination properties", capture.getvalue())
                self.assertEqual(calls[-1][0][:3], ["systemctl", "--user", "show"])
                self.assertFalse(cleanup_failure)
            return original_open(path, mode, *args, **kwargs)

        def check_output(command, **kwargs):
            if command == ["stat", "-f", "-c", "%T", "/sys/fs/cgroup"]:
                return "cgroup2fs\n"
            if command[:3] == ["systemctl", "--user", "show"]:
                return "TimeoutStopUSec=5s\nKillSignal=9\nKillMode=control-group\nRuntimeMaxUSec=15s\n"
            raise AssertionError(f"unexpected check_output: {command}")

        def run(command, **kwargs):
            nonlocal unit
            calls.append((command, kwargs))
            if command == ["systemctl", "--user", "show-environment"]:
                return subprocess.CompletedProcess(command, 0)
            if command[:1] == ["busctl"]:
                if "status" in command:
                    raise subprocess.CalledProcessError(
                        1, command, stderr="Failed to get credentials: No such device or address")
                self.assertEqual(command, PING)
                self.assertTrue(kwargs["check"])
                self.assertEqual(kwargs["timeout"], 15)
                if ping_failure:
                    raise subprocess.CalledProcessError(1, command, stderr="Ping failed")
                return subprocess.CompletedProcess(command, 0)
            if command[:1] == ["systemd-run"]:
                unit = next(value.removeprefix("--unit=") for value in command
                            if value.startswith("--unit="))
                if resource_timeout:
                    raise subprocess.TimeoutExpired(command, kwargs["timeout"])
                probe_output, probe_error = StringIO(), ""
                probe_index = command.index("-c") + 1
                with patch.object(sys, "argv", ["-c", *command[probe_index + 1:]]), \
                        redirect_stdout(probe_output):
                    try:
                        exec(compile(command[probe_index], "<resource-probe>", "exec"), {})
                    except RuntimeError as error:
                        probe_error = str(error)
                return subprocess.CompletedProcess(command, int(bool(probe_error)),
                                                   probe_output.getvalue(), probe_error)
            if command[:3] == ["systemctl", "--user", "stop"]:
                self.assertEqual(command[3], unit)
                return subprocess.CompletedProcess(command, int(cleanup_failure), "", "stop failed")
            if command[:3] == ["systemctl", "--user", "show"]:
                self.assertEqual(command[3], unit)
                return subprocess.CompletedProcess(command, 0,
                                                   "loaded\n" if cleanup_failure else "not-found\n", "")
            raise AssertionError(f"unexpected run: {command}")

        with tempfile.TemporaryDirectory() as directory:
            envfile = Path(directory) / "github-env"
            envfile.write_text(initial_env)
            with ExitStack() as stack:
                stack.enter_context(patch.dict(os.environ, {
                    "XDG_RUNTIME_DIR": str(runtime),
                    "DBUS_SESSION_BUS_ADDRESS": f"unix:path={runtime}/bus",
                    "GITHUB_ENV": str(envfile)}))
                for target, replacement in (("os.getuid", lambda: 1001),
                                            ("subprocess.run", run),
                                            ("subprocess.check_output", check_output)):
                    stack.enter_context(patch(target, replacement))
                for method, replacement in (("stat", path_stat), ("lstat", path_lstat),
                                            ("resolve", path_resolve), ("read_text", path_read),
                                            ("exists", path_exists), ("open", path_open)):
                    stack.enter_context(patch.object(Path, method, replacement))
                with redirect_stdout(capture), redirect_stderr(capture):
                    try:
                        exec(compile(SOURCE, str(SCRIPT), "exec"), {})
                        error = None
                    except (RuntimeError, subprocess.SubprocessError) as failure:
                        error = failure
                exported = envfile.read_text()
        return SimpleNamespace(error=error, calls=calls, env=exported,
                               output=capture.getvalue())

    def assert_scope_cleaned(self, result):
        commands = [command for command, _ in result.calls]
        self.assertEqual(sum(command[0] == "systemd-run" for command in commands), 1)
        self.assertEqual(sum(command[:3] == ["systemctl", "--user", "stop"]
                             for command in commands), 1)
        self.assertEqual(sum(command[:3] == ["systemctl", "--user", "show"]
                             for command in commands), 1)

    def test_credentials_diagnostic_is_irrelevant_and_resources_gate_export(self):
        result = self.run_preflight()
        self.assertIsNone(result.error, str(result.error))
        self.assert_scope_cleaned(result)
        commands = [command for command, _ in result.calls]
        self.assertEqual(commands[0], ["systemctl", "--user", "show-environment"])
        self.assertEqual(commands[1], PING)
        scope = commands[2]
        self.assertEqual(scope[:5], ["systemd-run", "--user", "--scope", "--quiet", "--collect"])
        self.assertTrue(scope[5].startswith("--unit=kio-confinement-preflight-"))
        self.assertEqual([value for value in scope if value.startswith("--property=")],
                         [f"--property={value}" for value in (
                             "MemoryMax=64M", "MemorySwapMax=0", "TasksMax=32", "CPUQuota=100%",
                             "OOMPolicy=kill", "RuntimeMaxSec=15s", "KillMode=control-group",
                             "KillSignal=SIGKILL", "TimeoutStopSec=5s")])
        self.assertIn("Verified resource controls and termination properties", result.output)
        self.assertEqual(result.env, "XDG_RUNTIME_DIR=/run/user/1001\n"
                         "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus\n")

    def test_ping_failure_refuses_before_scope_and_export(self):
        result = self.run_preflight(ping_failure=True)
        self.assertIsInstance(result.error, subprocess.CalledProcessError)
        self.assertEqual(result.error.cmd, PING)
        self.assertFalse(any(command[0] == "systemd-run" for command, _ in result.calls))
        self.assertEqual(result.env, "")

    def test_live_resource_limit_failure_cleans_scope_without_export(self):
        result = self.run_preflight(resource_failure=True)
        self.assertIsInstance(result.error, subprocess.CalledProcessError)
        self.assertIn("memory.max: expected 67108864, got 0", result.output)
        self.assert_scope_cleaned(result)
        self.assertEqual(result.env, "")

    def test_resource_timeout_cleans_scope_without_export(self):
        result = self.run_preflight(resource_timeout=True)
        self.assertIsInstance(result.error, subprocess.TimeoutExpired)
        self.assert_scope_cleaned(result)
        self.assertEqual(result.env, "")

    def test_cleanup_failure_refuses_export(self):
        result = self.run_preflight(cleanup_failure=True)
        self.assertIsInstance(result.error, RuntimeError)
        self.assertIn("could not clean up preflight scope", str(result.error))
        self.assert_scope_cleaned(result)
        self.assertEqual(result.env, "")

    def test_runtime_permissions_refuse_before_subprocess_and_export(self):
        result = self.run_preflight(runtime_mode=0o755)
        self.assertIsInstance(result.error, RuntimeError)
        self.assertIn("mode 0700", str(result.error))
        self.assertEqual(result.calls, [])
        self.assertEqual(result.env, "")

    def test_conflicting_environment_is_preserved_and_refused(self):
        initial = "XDG_RUNTIME_DIR=/wrong\n"
        result = self.run_preflight(initial_env=initial)
        self.assertIsInstance(result.error, RuntimeError)
        self.assertIn("conflicting XDG_RUNTIME_DIR", str(result.error))
        self.assert_scope_cleaned(result)
        self.assertEqual(result.env, initial)

    def test_successful_environment_export_is_idempotent(self):
        initial = "XDG_RUNTIME_DIR=/run/user/1001\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus\n"
        result = self.run_preflight(initial_env=initial)
        self.assertIsNone(result.error)
        self.assert_scope_cleaned(result)
        self.assertEqual(result.env, initial)


if __name__ == "__main__":
    unittest.main()
