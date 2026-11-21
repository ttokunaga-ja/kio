#!/usr/bin/env python3
"""Explicit-input Pillow/reportlab fixture renderer.

This adapter only renders the explicitly named image inputs to one explicitly
named PDF output.  Ground truth, evaluation, and artifact discovery belong to
Rust, not this adapter.
"""

from __future__ import annotations

import json
import os
import stat
import sys
import warnings
from hashlib import sha256
from pathlib import Path
from typing import Any

REQUEST_SCHEMA = "kio.ocr.fixture-render.request/v1"
RESPONSE_SCHEMA = "kio.ocr.fixture-render.response/v1"
# Rust owns the shared JSONL transport cap.  A 10k explicit-path request may
# legitimately exceed 256 KiB, but may never exceed this bounded 24 MiB lane.
MAX_REQUEST_BYTES = 24 * 1024 * 1024
MAX_INPUT_IMAGES = 10_000
MAX_IMAGE_BYTES = 16 * 1024 * 1024
MAX_TOTAL_BYTES = 64 * 1024 * 1024
MAX_IMAGE_PIXELS = 32 * 1024 * 1024
MAX_TOTAL_PIXELS = 96 * 1024 * 1024
MAX_OUTPUT_BYTES = 64 * 1024 * 1024


def read_request() -> dict[str, Any]:
    line = sys.stdin.buffer.readline(MAX_REQUEST_BYTES + 1)
    if not line or len(line) > MAX_REQUEST_BYTES or not line.endswith(b"\n") or sys.stdin.buffer.read(1):
        raise ValueError("expected one bounded JSONL request")
    request = json.loads(line)
    if set(request) != {"schema", "request_id", "output_pdf", "input_images"} or request["schema"] != REQUEST_SCHEMA:
        raise ValueError("unsupported renderer request schema")
    if not isinstance(request["output_pdf"], str) or not Path(request["output_pdf"]).is_absolute():
        raise ValueError("output_pdf must be absolute")
    if not isinstance(request["input_images"], list) or not request["input_images"] or len(request["input_images"]) > MAX_INPUT_IMAGES:
        raise ValueError("input_images must be a nonempty explicit list")
    if any(not isinstance(item, str) or not Path(item).is_absolute() for item in request["input_images"]):
        raise ValueError("input image paths must be absolute")
    if len(set(request["input_images"])) != len(request["input_images"]):
        raise ValueError("input image paths must be unique")
    return request


def reserve_output(output: Path) -> tuple[int, int, int, str]:
    """Open one output leaf without following or replacing anything.

    Rust chooses and prepares the parent directory. This adapter refuses to
    create it and retains the created inode identity for failure cleanup.
    """
    if not hasattr(os, "O_NOFOLLOW") or not hasattr(os, "O_DIRECTORY"):
        raise RuntimeError("safe create-only output requires O_NOFOLLOW and O_DIRECTORY")
    parent = output.parent
    if not parent.is_dir():
        raise ValueError("output parent must already exist")
    if output.name in {"", ".", ".."}:
        raise ValueError("output_pdf must name one file")
    parent_fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        output_fd = os.open(
            output.name,
            os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o600,
            dir_fd=parent_fd,
        )
    except BaseException:
        os.close(parent_fd)
        raise
    stat_result = os.fstat(output_fd)
    return parent_fd, output_fd, stat_result.st_dev, stat_result.st_ino


def remove_exact_created_output(parent_fd: int, name: str, device: int, inode: int) -> None:
    try:
        current = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        if current.st_dev == device and current.st_ino == inode:
            os.unlink(name, dir_fd=parent_fd)
    except FileNotFoundError:
        pass


def _no_clobber_preflight(existing_output: Path) -> bool:
    """Stdlib-only regression helper for exclusive-create behavior.

    It intentionally imports neither Pillow nor reportlab, so callers can
    verify the safety gate on environments that do not have render runtimes.
    """
    original = existing_output.read_bytes()
    try:
        reserve_output(existing_output)
    except FileExistsError:
        return existing_output.read_bytes() == original
    return False


def sha256_fd(fd: int) -> tuple[int, str]:
    os.lseek(fd, 0, os.SEEK_SET)
    digest = sha256()
    total = 0
    while chunk := os.read(fd, 1024 * 1024):
        total += len(chunk)
        digest.update(chunk)
    return total, digest.hexdigest()


def preflight_images(raw_paths: list[str]) -> None:
    """Reject decompression bombs before creating an output inode."""
    from PIL import Image

    Image.MAX_IMAGE_PIXELS = MAX_IMAGE_PIXELS
    total_bytes = 0
    total_pixels = 0
    with warnings.catch_warnings():
        warnings.simplefilter("error", Image.DecompressionBombWarning)
        for raw_path in raw_paths:
            image_path = Path(raw_path)
            file_stat = image_path.stat(follow_symlinks=False)
            if not stat.S_ISREG(file_stat.st_mode) or file_stat.st_size > MAX_IMAGE_BYTES:
                raise ValueError("input image exceeds regular-file byte bounds")
            total_bytes += file_stat.st_size
            if total_bytes > MAX_TOTAL_BYTES:
                raise ValueError("aggregate input image bytes exceed bound")
            with Image.open(image_path) as image:
                width, height = image.size
                pixels = width * height
                if width <= 0 or height <= 0 or pixels > MAX_IMAGE_PIXELS:
                    raise ValueError("input image pixels exceed bound")
                total_pixels += pixels
                if total_pixels > MAX_TOTAL_PIXELS:
                    raise ValueError("aggregate input image pixels exceed bound")
                image.verify()
            # verify() invalidates the object; `load()` forces bounded decode.
            with Image.open(image_path) as image:
                image.load()


class LimitedOutput:
    def __init__(self, output: Any) -> None:
        self.output = output
        self.total = 0

    def write(self, data: bytes) -> int:
        self.total += len(data)
        if self.total > MAX_OUTPUT_BYTES:
            raise RuntimeError("renderer output exceeds byte bound")
        return self.output.write(data)

    def flush(self) -> None:
        self.output.flush()


def main() -> None:
    request = read_request()
    output = Path(request["output_pdf"])
    try:
        # Keep Python-native libraries inside the rendering boundary, so the
        # no-clobber preflight is independently testable without them.
        preflight_images(request["input_images"])
        from reportlab.lib.pagesizes import A4
        from reportlab.lib.utils import ImageReader
        from reportlab.pdfgen.canvas import Canvas

        parent_fd, output_fd, device, inode = reserve_output(output)
        with os.fdopen(output_fd, "wb", closefd=False) as output_stream:
            canvas = Canvas(LimitedOutput(output_stream), pagesize=A4, invariant=True)
            width, height = A4
            for raw_path in request["input_images"]:
                image_path = Path(raw_path)
                canvas.drawImage(ImageReader(str(image_path)), 0, 0, width=width, height=height, preserveAspectRatio=True, anchor="c")
                canvas.showPage()
            canvas.save()
            output_stream.flush()
        os.fsync(output_fd)
        output_bytes, output_sha256 = sha256_fd(output_fd)
        if output_bytes == 0:
            raise RuntimeError("renderer produced an empty output")
        final = os.fstat(output_fd)
        if final.st_dev != device or final.st_ino != inode:
            raise RuntimeError("renderer output identity changed")
        response = {
            "schema": RESPONSE_SCHEMA,
            "request_id": request["request_id"],
            "output_pdf": str(output),
            "output_bytes": output_bytes,
            "output_sha256": output_sha256,
            "page_count": len(request["input_images"]),
        }
    except BaseException:
        if "output_fd" in locals():
            os.close(output_fd)
            remove_exact_created_output(parent_fd, output.name, device, inode)
            os.close(parent_fd)
        raise
    os.close(output_fd)
    os.close(parent_fd)
    sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"render_native: {error}", file=sys.stderr)
        raise SystemExit(1)
