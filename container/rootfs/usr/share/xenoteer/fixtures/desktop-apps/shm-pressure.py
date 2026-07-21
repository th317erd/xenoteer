#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Allocate and touch a deterministic POSIX shm file until terminated."""

import argparse
import json
import mmap
import os
from pathlib import Path
import signal


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bytes", type=int, default=512 * 1024 * 1024)
    parser.add_argument("--ready-file", type=Path, required=True)
    args = parser.parse_args()
    if args.bytes < 64 * 1024 * 1024 or args.bytes % mmap.PAGESIZE:
        raise SystemExit("pressure bytes must be page-aligned and at least 64 MiB")

    shm_path = Path(f"/dev/shm/xenoteer-fixture-pressure-{os.getpid()}")
    descriptor = os.open(shm_path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
    running = True

    def stop(_signum: int, _frame: object) -> None:
        nonlocal running
        running = False

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        os.ftruncate(descriptor, args.bytes)
        with mmap.mmap(descriptor, args.bytes, access=mmap.ACCESS_WRITE) as mapping:
            for offset in range(0, args.bytes, mmap.PAGESIZE):
                mapping[offset] = (offset // mmap.PAGESIZE) & 0xFF
            payload = {
                "type": "shm_pressure_ready",
                "bytes": args.bytes,
                "path": str(shm_path),
                "pid": os.getpid(),
            }
            args.ready_file.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
            print(json.dumps(payload, sort_keys=True), flush=True)
            while running:
                signal.pause()
    finally:
        os.close(descriptor)
        shm_path.unlink(missing_ok=True)
        args.ready_file.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
