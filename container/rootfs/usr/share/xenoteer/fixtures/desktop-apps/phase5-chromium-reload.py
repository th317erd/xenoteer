#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Reload the local Chromium fixture through its loopback-only DevTools endpoint."""

from __future__ import annotations

import argparse
import json
import os
import urllib.request
from pathlib import Path

import websocket


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", type=Path, required=True)
    args = parser.parse_args()

    active_port = args.profile / "DevToolsActivePort"
    fields = active_port.read_text(encoding="ascii").splitlines()
    if not fields or not fields[0].isdigit():
        raise SystemExit("Chromium DevToolsActivePort was malformed")
    port = int(fields[0])
    if port < 1 or port > 65_535:
        raise SystemExit("Chromium DevTools port was outside the TCP range")

    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(f"http://127.0.0.1:{port}/json/list", timeout=3) as response:
        payload = response.read(256 * 1024 + 1)
    if len(payload) > 256 * 1024:
        raise SystemExit("Chromium target listing exceeded its fixture bound")
    targets = json.loads(payload)
    page = next(
        (
            target
            for target in targets
            if target.get("type") == "page"
            and str(target.get("url", "")).startswith("file://")
        ),
        None,
    )
    if not isinstance(page, dict) or not isinstance(
        page.get("webSocketDebuggerUrl"), str
    ):
        raise SystemExit("Chromium fixture page target was unavailable")

    # The endpoint is loopback-only inside the fixture container. Suppress
    # inherited proxies so the reload cannot leave that boundary.
    for key in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"):
        os.environ.pop(key, None)
    connection = websocket.create_connection(
        page["webSocketDebuggerUrl"],
        timeout=3,
        http_proxy_host=None,
        # Chromium's DevTools endpoint is loopback-only and does not need a
        # browser Origin. Omitting it avoids version-dependent origin rejection
        # without weakening the browser launch contract with an allow-all flag.
        suppress_origin=True,
    )
    try:
        connection.send(json.dumps({"id": 1, "method": "Page.reload"}))
        response = json.loads(connection.recv())
    finally:
        connection.close()
    if response.get("id") != 1 or "error" in response:
        raise SystemExit("Chromium rejected the bounded fixture reload")
    print(json.dumps({"type": "phase5_chromium_reload_requested"}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
