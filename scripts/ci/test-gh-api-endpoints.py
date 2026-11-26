#!/usr/bin/env python3
"""Regression tests for the workflow gh api endpoint check."""
import pathlib
import subprocess
import sys
import tempfile
import unittest

CHECK = pathlib.Path(__file__).with_name("check-gh-api-endpoints.py")


class EndpointCheck(unittest.TestCase):
    def check_workflow(self, source):
        with tempfile.TemporaryDirectory() as temp:
            workflows = pathlib.Path(temp) / ".github/workflows"
            workflows.mkdir(parents=True)
            (workflows / "test.yml").write_text(source, encoding="utf-8")
            return subprocess.run([sys.executable, str(CHECK), temp], capture_output=True)

    def test_rejects_quoted_unquoted_and_continued_endpoints(self):
        for command in ("gh api /repos/a", "gh api '/repos/a'", 'gh api "/repos/a"',
                        'gh api --paginate \\\n  "/repos/a"'):
            with self.subTest(command=command):
                result = self.check_workflow(command)
                self.assertEqual(result.returncode, 1)
                self.assertIn(b"test.yml:1:", result.stderr)

    def test_accepts_relative_endpoints(self):
        self.assertEqual(self.check_workflow('gh api --paginate "repos/a"\n').returncode, 0)


if __name__ == "__main__":
    unittest.main()
