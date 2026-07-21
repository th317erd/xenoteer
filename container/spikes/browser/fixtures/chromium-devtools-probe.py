#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Prove Chromium rendered the X11 fixture and report chrome://sandbox."""

import base64
import json
import struct
import sys
import time
import urllib.request

import websocket


def fail(message: str) -> None:
    raise SystemExit(message)


def targets(port: int) -> list[dict[str, object]]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=3) as response:
        return json.load(response)


def receive_result(connection: websocket.WebSocket, command_id: int) -> dict[str, object]:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        message = json.loads(connection.recv())
        if message.get("id") == command_id:
            if "error" in message:
                fail(f"DevTools command {command_id} failed: {message['error']}")
            return message.get("result", {})
    fail(f"timed out waiting for DevTools command {command_id}")


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
    return receive_result(connection, command_id)


def evaluate(connection: websocket.WebSocket, command_id: int, expression: str) -> object:
    result = command(
        connection,
        command_id,
        "Runtime.evaluate",
        {"expression": expression, "returnByValue": True},
    )
    remote = result.get("result", {})
    if isinstance(remote, dict) and "value" in remote:
        return remote["value"]
    fail(f"JavaScript evaluation returned no value: {result}")


def wait_document(connection: websocket.WebSocket, first_id: int, marker: str | None) -> int:
    command_id = first_id
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        expression = "document.readyState + '|' + (document.body?.dataset?.xenoteerMarker || '')"
        value = str(evaluate(connection, command_id, expression))
        command_id += 1
        state, _, observed_marker = value.partition("|")
        if state == "complete" and (marker is None or observed_marker == marker):
            return command_id
        time.sleep(0.2)
    fail("Chromium document did not reach the expected rendered state")


def main() -> None:
    if len(sys.argv) not in (3, 4) or (len(sys.argv) == 4 and sys.argv[3] != "--skip-sandbox-status"):
        fail("usage: chromium-devtools-probe PORT SCREENSHOT [--skip-sandbox-status]")
    port = int(sys.argv[1])
    screenshot_path = sys.argv[2]
    check_sandbox_status = len(sys.argv) == 3
    page = next((item for item in targets(port) if item.get("type") == "page"), None)
    if not page or not isinstance(page.get("webSocketDebuggerUrl"), str):
        fail("Chromium exposed no debuggable page target")

    connection = websocket.create_connection(
        page["webSocketDebuggerUrl"], timeout=10, suppress_origin=True
    )
    try:
        command(connection, 1, "Runtime.enable")
        command(connection, 2, "Page.enable")
        command_id = wait_document(connection, 3, "phase0-rendered")
        rendered = evaluate(
            connection,
            command_id,
            "({title: document.title, marker: document.body.dataset.xenoteerMarker, "
            "background: getComputedStyle(document.body).backgroundColor})",
        )
        command_id += 1
        if rendered != {
            "title": "Xenoteer Phase 0 Browser Fixture",
            "marker": "phase0-rendered",
            "background": "rgb(20, 164, 77)",
        }:
            fail(f"unexpected Chromium DOM/render state: {rendered}")

        capture = command(connection, command_id, "Page.captureScreenshot", {"format": "png"})
        command_id += 1
        png = base64.b64decode(str(capture.get("data", "")), validate=True)
        if not png.startswith(b"\x89PNG\r\n\x1a\n") or len(png) < 24:
            fail("Chromium did not return a PNG screenshot")
        width, height = struct.unpack(">II", png[16:24])
        if width < 320 or height < 240:
            fail(f"Chromium screenshot is unexpectedly small: {width}x{height}")
        with open(screenshot_path, "wb") as output:
            output.write(png)

        normalized = "process-audited-externally"
        if check_sandbox_status:
            command(connection, command_id, "Page.navigate", {"url": "chrome://sandbox/"})
            command_id += 1
            command_id = wait_document(connection, command_id, None)
            sandbox_text = str(evaluate(connection, command_id, "document.body.innerText"))
            normalized = " ".join(sandbox_text.split())
            required = (
                "PID namespaces Yes",
                "Network namespaces Yes",
                "Seccomp-BPF sandbox Yes",
                "You are adequately sandboxed.",
            )
            missing = [status for status in required if status not in normalized]
            if missing:
                fail(f"Chromium sandbox status missing {missing}: {normalized}")
        print(
            json.dumps(
                {
                    "type": "chromium_spike_ready",
                    "screenshot": {"width": width, "height": height, "bytes": len(png)},
                    "sandbox": normalized,
                },
                sort_keys=True,
            )
        )
    finally:
        connection.close()


if __name__ == "__main__":
    main()
