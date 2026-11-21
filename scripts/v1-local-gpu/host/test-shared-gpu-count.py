"""Exercise socket selection without a host Docker service."""
import contextlib
import io
from pathlib import Path
import runpy
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch


HELPER = runpy.run_path(str(Path(__file__).with_name("shared-gpu-count")))
MAIN = HELPER["main"]


class SharedGpuCountTests(unittest.TestCase):
    def test_only_the_configured_socket_is_queried(self):
        for endpoint in ("/run/docker.sock", "/mnt/wsl/docker-desktop/docker.sock"):
            with self.subTest(endpoint=endpoint):
                output = io.StringIO()
                with (
                    patch.dict(MAIN.__globals__, {"configured_socket": lambda _: endpoint}),
                    patch("os.geteuid", return_value=0),
                    patch("signal.alarm"),
                    patch("sys.argv", ["shared-gpu-count"]),
                    patch.dict("os.environ", {"DOCKER_HOST": "unix:///tmp/ignored.sock"}),
                    patch.dict(MAIN.__globals__, {"running_requests": lambda _: 1}),
                    contextlib.redirect_stdout(output),
                ):
                    with patch.dict(MAIN.__globals__, {"Engine": Mock()}) as scope:
                        self.assertEqual(MAIN(), 0)
                        scope["Engine"].assert_called_once_with(endpoint, timeout=4)
                self.assertEqual(output.getvalue(), "1\n")

    def test_unavailable_endpoint_fails_without_output(self):
        for stage, error in (("configured_socket", FileNotFoundError), ("running_requests", ConnectionRefusedError)):
            scope = {"configured_socket": lambda _: "/run/docker.sock", stage: Mock(side_effect=error)}
            with (
                self.subTest(error=error),
                patch.dict(MAIN.__globals__, scope),
                patch("os.geteuid", return_value=0),
                patch("signal.alarm"),
                patch("sys.argv", ["shared-gpu-count"]),
                contextlib.redirect_stderr(io.StringIO()),
                contextlib.redirect_stdout(io.StringIO()) as output,
            ):
                self.assertEqual(MAIN(), 1)
                self.assertEqual(output.getvalue(), "")

    def test_counts_only_running_containers_with_device_requests(self):
        ids = ["a" * 64, "b" * 64, "c" * 64]
        engine = Mock()
        engine.read_json.side_effect = [
            [{"Id": value} for value in ids],
            {"State": {"Running": True}, "HostConfig": {"DeviceRequests": [{"Capabilities": [["gpu"]]}]}},
            {"State": {"Running": False}, "HostConfig": {"DeviceRequests": [{}]}},
            {"State": {"Running": True}, "HostConfig": {"DeviceRequests": None}},
        ]
        self.assertEqual(HELPER["running_requests"](engine), 1)

    def test_config_requires_one_absolute_path_and_private_root_metadata(self):
        check = HELPER["configured_socket"]
        with tempfile.TemporaryDirectory() as working:
            config = Path(working) / "socket"
            for raw, mode, uid in (
                (b"/run/docker.sock\n/mnt/wsl/docker.sock\n", 0o600, 0),
                (b"/run/../tmp/docker.sock\n", 0o600, 0),
                (b"/run/docker.sock\n", 0o644, 0),
                (b"/run/docker.sock\n", 0o600, 1000),
            ):
                with self.subTest(raw=raw, mode=mode, uid=uid):
                    config.write_bytes(raw)
                    info = SimpleNamespace(st_mode=stat.S_IFREG | mode, st_uid=uid)
                    with patch("os.fstat", return_value=info), self.assertRaises(ValueError):
                        check(config)


if __name__ == "__main__":
    unittest.main()
