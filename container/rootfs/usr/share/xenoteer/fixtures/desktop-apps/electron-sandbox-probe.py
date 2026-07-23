#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Prove the Electron page is a local, context-isolated non-Node renderer."""

import argparse
import json
import urllib.request

import websocket


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("port", type=int)
    args = parser.parse_args()
    with urllib.request.urlopen(
        f"http://127.0.0.1:{args.port}/json/list", timeout=3
    ) as response:
        targets = json.load(response)
    page = next((item for item in targets if item.get("type") == "page"), None)
    if not page or not isinstance(page.get("webSocketDebuggerUrl"), str):
        raise SystemExit("Electron exposed no debuggable page")

    connection = websocket.create_connection(
        page["webSocketDebuggerUrl"], timeout=10, suppress_origin=True
    )
    try:
        connection.send(
            json.dumps(
                {
                    "id": 1,
                    "method": "Runtime.evaluate",
                    "params": {
                        "expression": """({
                          processType: typeof process,
                          requireType: typeof require,
                          protocol: location.protocol,
                          marker: document.getElementById('browser-marker')?.textContent,
                          title: document.title
                        })""",
                        "returnByValue": True,
                    },
                }
            )
        )
        while True:
            response = json.loads(connection.recv())
            if response.get("id") == 1:
                break
        if "error" in response:
            raise SystemExit(f"Electron DevTools evaluation failed: {response['error']}")
        observed = response.get("result", {}).get("result", {}).get("value")
        expected = {
            "processType": "undefined",
            "requireType": "undefined",
            "protocol": "file:",
            "marker": "Electron fixture marker content",
            "title": "Xenoteer Electron Browser Fixture",
        }
        if observed != expected:
            raise SystemExit(
                f"Electron renderer isolation differs: observed={observed!r}, expected={expected!r}"
            )
        print(
            json.dumps(
                {"type": "electron_renderer_isolation", "ok": True, **observed},
                sort_keys=True,
            ),
            flush=True,
        )
        return 0
    finally:
        connection.close()


if __name__ == "__main__":
    raise SystemExit(main())
