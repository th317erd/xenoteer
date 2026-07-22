#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Focus or verify one named editable control without printing its contents."""

import argparse
import hashlib
import json
import time
from pathlib import Path

import pyatspi


def editable_named(name: str, limit: int = 20_000):
    pending = [(pyatspi.Registry.getDesktop(0), 0)]
    visited = 0
    while pending and visited < limit:
        node, depth = pending.pop()
        visited += 1
        try:
            if str(node.name) == name and node.getState().contains(pyatspi.STATE_EDITABLE):
                node.queryText()
                node.queryComponent()
                yield node
            if depth < 40:
                count = min(int(node.childCount), 2_000)
                pending.extend(
                    (node.getChildAtIndex(index), depth + 1) for index in range(count)
                )
        except Exception:  # noqa: BLE001 - remote AT-SPI objects may vanish.
            continue


def text_value(node) -> str:
    text = node.queryText()
    return str(text.getText(0, int(text.characterCount)))


def focus(name: str, deadline: float) -> dict[str, object] | None:
    for node in editable_named(name):
        try:
            if not node.queryComponent().grabFocus():
                continue
            text = node.queryText()
            if not text.setCaretOffset(int(text.characterCount)):
                continue
            while time.monotonic() < deadline:
                if node.getState().contains(pyatspi.STATE_FOCUSED):
                    return {"type": "phase4_atspi_focused", "name": name}
                time.sleep(0.05)
        except Exception:  # noqa: BLE001 - retry after transient tree changes.
            continue
    return None


def assert_text(name: str, expected: str) -> dict[str, object] | None:
    for node in editable_named(name):
        try:
            actual = text_value(node)
        except Exception:  # noqa: BLE001 - retry after transient tree changes.
            continue
        if actual == expected:
            encoded = actual.encode("utf-8")
            return {
                "type": "phase4_atspi_text_verified",
                "name": name,
                "utf8_bytes": len(encoded),
                "sha256": hashlib.sha256(encoded).hexdigest(),
            }
    return None


def text_evidence(name: str) -> list[dict[str, object]]:
    """Return bounded content-free evidence for failed text convergence."""
    evidence = []
    for node in editable_named(name):
        try:
            encoded = text_value(node).encode("utf-8")
        except Exception:  # noqa: BLE001 - remote AT-SPI objects may vanish.
            continue
        evidence.append(
            {
                "utf8_bytes": len(encoded),
                "sha256": hashlib.sha256(encoded).hexdigest(),
            }
        )
        if len(evidence) == 8:
            break
    return evidence


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("operation", choices=("focus", "assert-text"))
    parser.add_argument("--name", required=True)
    parser.add_argument("--expected-file", type=Path)
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()
    if args.operation == "assert-text" and args.expected_file is None:
        parser.error("assert-text requires --expected-file")
    if args.operation == "focus" and args.expected_file is not None:
        parser.error("focus does not accept --expected-file")

    expected = None
    if args.expected_file is not None:
        expected = args.expected_file.read_text(encoding="utf-8")
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        result = (
            focus(args.name, deadline)
            if args.operation == "focus"
            else assert_text(args.name, expected or "")
        )
        if result is not None:
            print(json.dumps(result, sort_keys=True), flush=True)
            return 0
        time.sleep(0.2)
    if args.operation == "assert-text":
        expected_bytes = (expected or "").encode("utf-8")
        detail = {
            "expected_utf8_bytes": len(expected_bytes),
            "expected_sha256": hashlib.sha256(expected_bytes).hexdigest(),
            "observed": text_evidence(args.name),
        }
        raise SystemExit(
            "named editable control did not satisfy 'assert-text': "
            + json.dumps(detail, sort_keys=True)
        )
    raise SystemExit(f"named editable control did not satisfy {args.operation!r}")


if __name__ == "__main__":
    raise SystemExit(main())
