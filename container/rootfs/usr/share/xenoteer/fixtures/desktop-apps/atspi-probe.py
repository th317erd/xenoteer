#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Wait for stable AT-SPI names without trusting fixture-ready output."""

import argparse
import json
import time

import pyatspi


def snapshot_names(limit: int = 20_000) -> set[str]:
    names: set[str] = set()
    pending = [(pyatspi.Registry.getDesktop(0), 0)]
    visited = 0
    while pending and visited < limit:
        node, depth = pending.pop()
        visited += 1
        try:
            name = node.name
        except Exception:  # noqa: BLE001 - remote AT-SPI objects may vanish.
            continue
        if name:
            names.add(str(name))
        if depth >= 40:
            continue
        try:
            count = min(int(node.childCount), 2_000)
            pending.extend((node.getChildAtIndex(index), depth + 1) for index in range(count))
        except Exception:  # noqa: BLE001 - tree mutation is expected while polling.
            continue
    return names


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--name", action="append", default=[])
    parser.add_argument("--absent-name", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()

    required = set(args.name)
    forbidden = set(args.absent_name)
    if not required and not forbidden:
        parser.error("at least one --name or --absent-name is required")
    deadline = time.monotonic() + args.timeout
    last_names: set[str] = set()
    while time.monotonic() < deadline:
        last_names = snapshot_names()
        if required <= last_names and forbidden.isdisjoint(last_names):
            print(
                json.dumps(
                    {
                        "type": "atspi_names_observed",
                        "names": sorted(required),
                        "absent_names": sorted(forbidden),
                    },
                    sort_keys=True,
                ),
                flush=True,
            )
            return 0
        time.sleep(0.25)
    missing = sorted(required - last_names)
    present = sorted(forbidden & last_names)
    raise SystemExit(
        f"AT-SPI names did not converge before timeout: missing={missing!r}, "
        f"still_present={present!r}"
    )


if __name__ == "__main__":
    raise SystemExit(main())
