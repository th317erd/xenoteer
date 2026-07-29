#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Standard JSON stdin/stdout adapter for the Xenoteer v1 corpus."""

from __future__ import annotations

import json
import sys
from pathlib import Path


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PACKAGE_ROOT / "src"))


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, UnicodeDecodeError):
        print("adapter input is not JSON", file=sys.stderr)
        return 2
    if (
        not isinstance(payload, dict)
        or payload.get("adapter_protocol") != 1
        or not isinstance(payload.get("cases"), list)
    ):
        print("adapter input has an unsupported shape", file=sys.stderr)
        return 2
    from xenoteer.conformance import (
        FROZEN_CORPUS,
        FROZEN_CORPUS_SHA256,
        FROZEN_PROTOCOL,
        run_cases,
    )

    if (
        payload.get("corpus") != FROZEN_CORPUS
        or payload.get("corpus_sha256") != FROZEN_CORPUS_SHA256
        or payload.get("protocol") != FROZEN_PROTOCOL
    ):
        print("adapter input has an unsupported corpus identity", file=sys.stderr)
        return 2

    results = run_cases(payload["cases"])
    json.dump(
        {
            "adapter_protocol": 1,
            "results": [
                {
                    "detail": result.detail,
                    "id": result.case_id,
                    "status": result.status,
                }
                for result in results
            ],
        },
        sys.stdout,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
