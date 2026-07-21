#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Read Chromium's own chrome://sandbox verdict through loopback DevTools."""

import argparse
import json
import time
import urllib.request

import websocket


def targets(port: int) -> list[dict[str, object]]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=3) as response:
        return json.load(response)


def receive(connection: websocket.WebSocket, command_id: int) -> dict[str, object]:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        payload = json.loads(connection.recv())
        if payload.get("id") != command_id:
            continue
        if "error" in payload:
            raise SystemExit(f"DevTools command failed: {payload['error']}")
        return payload.get("result", {})
    raise SystemExit("DevTools response timed out")


def command(
    connection: websocket.WebSocket,
    command_id: int,
    method: str,
    params: dict[str, object] | None = None,
) -> dict[str, object]:
    payload: dict[str, object] = {"id": command_id, "method": method}
    if params is not None:
        payload["params"] = params
    connection.send(json.dumps(payload))
    return receive(connection, command_id)


def evaluate(connection: websocket.WebSocket, command_id: int, expression: str) -> object:
    result = command(
        connection,
        command_id,
        "Runtime.evaluate",
        {"expression": expression, "returnByValue": True},
    )
    remote = result.get("result", {})
    if not isinstance(remote, dict) or "value" not in remote:
        raise SystemExit(f"DevTools evaluation returned no value: {result!r}")
    return remote["value"]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("port", type=int)
    args = parser.parse_args()

    page = next((item for item in targets(args.port) if item.get("type") == "page"), None)
    if not page or not isinstance(page.get("webSocketDebuggerUrl"), str):
        raise SystemExit("Chromium exposed no debuggable page")

    connection = websocket.create_connection(
        page["webSocketDebuggerUrl"], timeout=10, suppress_origin=True
    )
    try:
        command(connection, 1, "Runtime.enable")
        command(connection, 2, "Page.enable")
        command(connection, 3, "Page.navigate", {"url": "chrome://sandbox/"})
        deadline = time.monotonic() + 15
        sandbox_text = ""
        command_id = 4
        while time.monotonic() < deadline:
            state = str(evaluate(connection, command_id, "document.readyState"))
            command_id += 1
            sandbox_text = str(evaluate(connection, command_id, "document.body?.innerText || ''"))
            command_id += 1
            if state == "complete" and sandbox_text:
                break
            time.sleep(0.2)
        normalized = " ".join(sandbox_text.split())
        required = (
            "PID namespaces Yes",
            "Network namespaces Yes",
            "Seccomp-BPF sandbox Yes",
            "You are adequately sandboxed.",
        )
        missing = [status for status in required if status not in normalized]
        if missing:
            raise SystemExit(f"Chromium sandbox status missing {missing!r}: {normalized}")
        print(
            json.dumps(
                {"type": "chromium_sandbox_status", "ok": True, "required": required},
                sort_keys=True,
            ),
            flush=True,
        )
        return 0
    finally:
        connection.close()


if __name__ == "__main__":
    raise SystemExit(main())
