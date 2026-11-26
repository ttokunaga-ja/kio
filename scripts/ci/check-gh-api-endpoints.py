#!/usr/bin/env python3
"""Reject gh api endpoint arguments that MSYS can rewrite as paths."""
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else ".")
pattern = re.compile(r"\bgh\s+api\b[^\n]*?(?:^|\s)[\"']?/(?=[^\s\"'])")
invalid = False
for path in sorted((root / ".github/workflows").glob("*.y*ml")):
    text = path.read_text(encoding="utf-8")
    # Shell continuations belong to the same command, but keep newlines for diagnostics.
    joined = re.sub(r"\\\n[ \t]*", lambda m: " " * len(m.group()), text)
    for match in pattern.finditer(joined):
        line = text.count("\n", 0, match.start()) + 1
        print(f"{path}:{line}: gh api endpoint starts with /", file=sys.stderr)
        invalid = True
sys.exit(invalid)
