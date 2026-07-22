#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Deterministic GTK/X11 clipboard owner and consumer with redacted output."""

import argparse
import hashlib
import json
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402


def evidence(kind: str, body: bytes) -> dict[str, object]:
    return {
        "type": kind,
        "utf8_bytes": len(body),
        "sha256": hashlib.sha256(body).hexdigest(),
    }


def own(input_path: Path, ready_path: Path) -> int:
    body = input_path.read_bytes()
    text = body.decode("utf-8")
    clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
    clipboard.set_text(text, -1)

    def publish_ready() -> bool:
        if not clipboard.wait_is_text_available():
            return True
        payload = evidence("phase4_clipboard_owner_ready", body)
        ready_path.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps(payload, sort_keys=True), flush=True)
        return False

    GLib.timeout_add(25, publish_ready)
    Gtk.main()
    return 0


def read(expected_path: Path) -> int:
    expected = expected_path.read_bytes()
    expected.decode("utf-8")
    clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
    actual = clipboard.wait_for_text()
    if actual is None or actual.encode("utf-8") != expected:
        raise SystemExit("clipboard content did not match expected digest and length")
    print(json.dumps(evidence("phase4_clipboard_read_verified", expected), sort_keys=True))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="operation", required=True)
    owner = subparsers.add_parser("own")
    owner.add_argument("--input", type=Path, required=True)
    owner.add_argument("--ready-file", type=Path, required=True)
    reader = subparsers.add_parser("read")
    reader.add_argument("--expected", type=Path, required=True)
    args = parser.parse_args()
    if args.operation == "own":
        return own(args.input, args.ready_file)
    return read(args.expected)


if __name__ == "__main__":
    raise SystemExit(main())
