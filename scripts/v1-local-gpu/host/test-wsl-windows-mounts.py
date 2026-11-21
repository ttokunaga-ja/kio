"""Exercise WSL mount policy with fixtures without running host setup."""
import os
from pathlib import Path
import runpy
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


HELPER = runpy.run_path(str(Path(__file__).with_name("wsl-windows-mounts.py")))


def mount(path, filesystem, dev="0:30"):
    return f"42 1 {dev} / {path} rw - {filesystem} source rw\n"


class WindowsMountTests(unittest.TestCase):
    def test_types_and_nested_mounts(self):
        inventory = HELPER["windows_mounts"](
            mount("/Docker/host", "9p") + mount("/Docker/host/nested", "virtiofs")
            + mount("/mnt/c", "drvfs") + mount("/extra/One\\040Drive", "fuse.virtiofs")
            + mount("/mnt/wsl", "tmpfs") + mount("/Docker/layers", "ext4")
        )
        self.assertEqual(list(inventory), ["/Docker/host", "/Docker/host/nested", "/extra/One Drive", "/mnt/c"])
        with patch("os.path.isdir", return_value=True):
            dirs, files = HELPER["masks"](inventory)
        self.assertEqual(dirs, ["/Docker/host", "/extra/One Drive", "/mnt/c"])
        self.assertEqual(files, [])

    def test_systemd_policy_quotes_paths_and_handles_files(self):
        lines = HELPER["policy"](["/Docker/Windows Share%"], ["/alias"])
        self.assertEqual(lines, [
            'TemporaryFileSystem="/Docker/Windows Share%%:ro"',
            'InaccessiblePaths="/alias"',
        ])

    def test_verify_rejects_a_visible_or_writable_mask(self):
        verify = HELPER["verify"]
        with (
            patch("os.access", return_value=False),
            patch("os.stat", return_value=SimpleNamespace(st_dev=os.makedev(0, 30))),
            patch("os.statvfs", return_value=SimpleNamespace(f_flag=1)),
            patch("os.listdir", return_value=[]),
        ):
            with self.assertRaisesRegex(ValueError, "tmpfs"):
                verify(["/Docker/host"], ["/Docker/host"], [], "", mount("/Docker/host", "9p"))
            verify(["/Docker/host"], ["/Docker/host"], [], "", mount("/Docker/host", "tmpfs"))
            with patch("os.access", return_value=True), self.assertRaisesRegex(ValueError, "writable"):
                verify(["/Docker/host"], ["/Docker/host"], [], "", mount("/Docker/host", "tmpfs"))
            with patch("os.statvfs", return_value=SimpleNamespace(f_flag=0)), self.assertRaisesRegex(ValueError, "read-only"):
                verify(["/Docker/host"], ["/Docker/host"], [], "", mount("/Docker/host", "tmpfs"))
            with self.assertRaisesRegex(ValueError, "still visible"):
                verify([], [], [], "", mount("/new/mount", "virtiofs"))

    def test_only_the_configured_socket_may_appear(self):
        verify = HELPER["verify"]
        with (
            patch("os.access", return_value=False),
            patch("os.stat", return_value=SimpleNamespace(st_dev=os.makedev(0, 30), st_mode=stat.S_IFSOCK)),
            patch("os.statvfs", return_value=SimpleNamespace(f_flag=1)),
            patch("os.listdir", side_effect=lambda path: ["proxy"] if path == "/Docker/host" else ["docker.sock"]),
        ):
            verify(["/Docker/host"], ["/Docker/host"], [], "/Docker/host/proxy/docker.sock", mount("/Docker/host", "tmpfs"))
            with self.assertRaisesRegex(ValueError, "entries"):
                verify(["/Docker/host"], ["/Docker/host"], [], "", mount("/Docker/host", "tmpfs"))

    def test_nested_target_can_be_absent(self):
        def stat_target(path):
            if path.endswith("/nested"):
                raise FileNotFoundError(path)
            return SimpleNamespace(st_dev=os.makedev(0, 30))

        with (
            patch("os.access", return_value=False),
            patch("os.stat", side_effect=stat_target),
            patch("os.statvfs", return_value=SimpleNamespace(f_flag=1)),
            patch("os.listdir", return_value=[]),
        ):
            HELPER["verify"](
                ["/Docker/host", "/Docker/host/nested"], ["/Docker/host"], [], "", mount("/Docker/host", "tmpfs"),
            )

    def test_file_mask_must_block_read_and_write(self):
        with (
            patch("os.access", side_effect=lambda path, mode: mode == os.R_OK),
            patch("os.stat", return_value=SimpleNamespace(st_dev=os.makedev(0, 30))),
            self.assertRaisesRegex(ValueError, "file mount is accessible"),
        ):
            HELPER["verify"](["/alias"], [], ["/alias"], "", mount("/alias", "tmpfs"))

    def test_cdi_stages_only_referenced_driver_files(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory, "stage")
            stage.mkdir()
            edit = {"mounts": [
                {"hostPath": "/usr/lib/wsl/drivers/nv_driver/nvidia-smi",
                 "containerPath": "/usr/lib/wsl/drivers/nv_driver/nvidia-smi", "options": ["ro"]},
                {"hostPath": "/usr/lib/wsl/lib/libdxcore.so", "containerPath": "/driver/libdxcore.so"},
            ]}
            with (
                patch.dict(HELPER["stage_cdi"].__globals__, {"trusted_driver_file": lambda path: path}),
                patch("shutil.copyfile") as copy,
                patch("os.chmod"),
            ):
                HELPER["stage_cdi"]({"containerEdits": edit, "devices": []}, ["/usr/lib/wsl/drivers"], stage, "/opt/kio/cdi")
            copy.assert_called_once_with(Path("/usr/lib/wsl/drivers/nv_driver/nvidia-smi"), stage / "0")
            self.assertEqual(edit["mounts"][0]["hostPath"], "/opt/kio/cdi/0")
            self.assertEqual(edit["mounts"][0]["containerPath"], "/usr/lib/wsl/drivers/nv_driver/nvidia-smi")
            self.assertEqual(edit["mounts"][1]["hostPath"], "/usr/lib/wsl/lib/libdxcore.so")

    def test_cdi_refuses_a_host_share_reference(self):
        with self.assertRaisesRegex(ValueError, "driver file"):
            HELPER["trusted_driver_file"](Path("/Docker/host/file.so"))


if __name__ == "__main__":
    unittest.main()
