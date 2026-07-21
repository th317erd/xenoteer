#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Exercise real RFC 6455 and RFB 3.8 bytes against the spike services."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import time
import urllib.parse
import urllib.request


MAX_WEBSOCKET_PAYLOAD = 16 * 1024 * 1024
MAX_RFB_TEXT = 1024 * 1024
MAX_RFB_NAME = 4096
MAX_RFB_RECTANGLES = 1024
MAX_FRAMEBUFFER_BYTES = 16 * 1024 * 1024


class ProbeError(RuntimeError):
    """A protocol or evidence assertion failed."""


class WebSocket:
    def __init__(
        self,
        host: str,
        port: int,
        path: str,
        protocol: str | None = "binary",
        origin: str | None = "http://localhost",
    ) -> None:
        self._socket = socket.create_connection((host, port), timeout=5.0)
        self._socket.settimeout(5.0)
        self._network_buffer = bytearray()
        self._rfb_buffer = bytearray()
        self._upgrade(host, port, path, protocol, origin)

    def _upgrade(
        self,
        host: str,
        port: int,
        path: str,
        protocol: str | None,
        origin: str | None,
    ) -> None:
        nonce = os.urandom(16)
        key = base64.b64encode(nonce).decode("ascii")
        request_lines = [
            f"GET {path} HTTP/1.1",
            f"Host: {host}:{port}",
            "Upgrade: websocket",
            "Connection: Upgrade",
            f"Sec-WebSocket-Key: {key}",
            "Sec-WebSocket-Version: 13",
        ]
        if protocol is not None:
            request_lines.append(f"Sec-WebSocket-Protocol: {protocol}")
        if origin is not None:
            request_lines.append(f"Origin: {origin}")
        request = ("\r\n".join(request_lines) + "\r\n\r\n").encode("ascii")
        self._socket.sendall(request)
        response = bytearray()
        while b"\r\n\r\n" not in response:
            chunk = self._socket.recv(4096)
            if not chunk:
                raise ProbeError("websockify closed during HTTP upgrade")
            response.extend(chunk)
            if len(response) > 65536:
                raise ProbeError("oversized WebSocket upgrade response")
        header, remainder = bytes(response).split(b"\r\n\r\n", 1)
        lines = header.decode("ascii").split("\r\n")
        if not lines[0].startswith("HTTP/1.1 101 "):
            raise ProbeError(f"WebSocket upgrade failed: {lines[0]}")
        headers: dict[str, str] = {}
        for line in lines[1:]:
            name, value = line.split(":", 1)
            headers[name.lower()] = value.strip()
        expected = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()
        ).decode("ascii")
        if headers.get("sec-websocket-accept") != expected:
            raise ProbeError("invalid Sec-WebSocket-Accept")
        if protocol is not None and headers.get("sec-websocket-protocol") != protocol:
            raise ProbeError(f"WebSocket did not negotiate the {protocol} subprotocol")
        self._network_buffer.extend(remainder)

    def _recv_tcp(self, length: int) -> bytes:
        if length < 0 or length > MAX_WEBSOCKET_PAYLOAD:
            raise ProbeError(f"refusing bounded read of {length} bytes")
        while len(self._network_buffer) < length:
            chunk = self._socket.recv(max(4096, length - len(self._network_buffer)))
            if not chunk:
                raise ProbeError("WebSocket closed unexpectedly")
            self._network_buffer.extend(chunk)
        value = bytes(self._network_buffer[:length])
        del self._network_buffer[:length]
        return value

    def _recv_frame(self) -> tuple[bool, int, bytes]:
        first, second = self._recv_tcp(2)
        if first & 0x70:
            raise ProbeError("unexpected WebSocket RSV bits")
        final = bool(first & 0x80)
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        if length == 126:
            length = struct.unpack(">H", self._recv_tcp(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self._recv_tcp(8))[0]
        if length > MAX_WEBSOCKET_PAYLOAD:
            raise ProbeError(f"WebSocket payload exceeds {MAX_WEBSOCKET_PAYLOAD} bytes")
        mask = self._recv_tcp(4) if masked else b""
        payload = self._recv_tcp(length)
        if masked:
            payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        return final, opcode, payload

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        length = len(payload)
        if length < 126:
            header = bytes((0x80 | opcode, 0x80 | length))
        elif length <= 65535:
            header = bytes((0x80 | opcode, 0xFE)) + struct.pack(">H", length)
        else:
            header = bytes((0x80 | opcode, 0xFF)) + struct.pack(">Q", length)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        self._socket.sendall(header + mask + masked)

    def send_binary(self, payload: bytes) -> None:
        self._send_frame(0x2, payload)

    def send_text(self, payload: str) -> None:
        self._send_frame(0x1, payload.encode("utf-8"))

    def recv_message(self) -> tuple[int, bytes]:
        while True:
            final, opcode, payload = self._recv_frame()
            if opcode == 0x8:
                raise ProbeError("WebSocket closed unexpectedly")
            if opcode == 0x9:
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode not in (0x1, 0x2):
                raise ProbeError(f"unexpected WebSocket opcode {opcode}")
            if not final:
                fragments = bytearray(payload)
                while not final:
                    final, continuation, payload = self._recv_frame()
                    if continuation != 0x0:
                        raise ProbeError("invalid fragmented WebSocket message")
                    fragments.extend(payload)
                payload = bytes(fragments)
            return opcode, payload

    def recv_rfb(self, length: int) -> bytes:
        while len(self._rfb_buffer) < length:
            final, opcode, payload = self._recv_frame()
            if opcode == 0x8:
                raise ProbeError("WebSocket sent a close frame during RFB")
            if opcode == 0x9:
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode not in (0x0, 0x2):
                raise ProbeError(f"unexpected WebSocket opcode {opcode}")
            if not final:
                fragments = bytearray(payload)
                while not final:
                    final, continuation, payload = self._recv_frame()
                    if continuation != 0x0:
                        raise ProbeError("invalid fragmented binary message")
                    fragments.extend(payload)
                payload = bytes(fragments)
            self._rfb_buffer.extend(payload)
        value = bytes(self._rfb_buffer[:length])
        del self._rfb_buffer[:length]
        return value

    def close(self) -> None:
        try:
            self._send_frame(0x8, struct.pack(">H", 1000))
        finally:
            self._socket.close()


def wait_port(host: str, port: int, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise ProbeError(f"{host}:{port} did not accept connections")


def assert_loopback(ports: list[int]) -> None:
    wanted = {f"{port:04X}" for port in ports}
    found: dict[str, set[str]] = {port: set() for port in wanted}
    with open("/proc/net/tcp", "r", encoding="ascii") as sockets:
        next(sockets)
        for line in sockets:
            fields = line.split()
            address, port = fields[1].split(":")
            if fields[3] == "0A" and port in wanted:
                found[port].add(address)
    for port in wanted:
        if found[port] != {"0100007F"}:
            raise ProbeError(f"TCP port {int(port, 16)} listeners were {found[port]}")
    ipv6_loopback = "00000000000000000000000001000000"
    with open("/proc/net/tcp6", "r", encoding="ascii") as sockets:
        next(sockets)
        for line in sockets:
            fields = line.split()
            address, port = fields[1].split(":")
            if fields[3] == "0A" and port in wanted and address != ipv6_loopback:
                raise ProbeError(
                    f"viewer service opened non-loopback IPv6 listener {address}:{port}"
                )


def assert_png(path: str, width: int, height: int) -> None:
    with open(path, "rb") as screenshot:
        header = screenshot.read(24)
        screenshot.seek(0, os.SEEK_END)
        size = screenshot.tell()
    if len(header) != 24 or header[:8] != b"\x89PNG\r\n\x1a\n":
        raise ProbeError("Chromium noVNC screenshot is not a PNG")
    actual_width, actual_height = struct.unpack(">II", header[16:24])
    if (actual_width, actual_height) != (width, height):
        raise ProbeError(
            f"Chromium screenshot is {actual_width}x{actual_height}, expected {width}x{height}"
        )
    if size < 4096:
        raise ProbeError(f"Chromium noVNC screenshot is implausibly small ({size} bytes)")


def devtools_command(
    websocket: WebSocket,
    command_id: int,
    method: str,
    params: dict[str, object] | None = None,
) -> dict[str, object]:
    payload: dict[str, object] = {"id": command_id, "method": method}
    if params is not None:
        payload["params"] = params
    websocket.send_text(json.dumps(payload, separators=(",", ":")))
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        opcode, raw_message = websocket.recv_message()
        if opcode != 0x1:
            continue
        message = json.loads(raw_message)
        if message.get("id") != command_id:
            continue
        if "error" in message:
            raise ProbeError(f"DevTools command {method} failed: {message['error']}")
        result = message.get("result", {})
        if not isinstance(result, dict):
            raise ProbeError(f"DevTools command {method} returned a non-object result")
        return result
    raise ProbeError(f"timed out waiting for DevTools command {method}")


def run_chromium_probe(port: int, screenshot_path: str) -> dict[str, object]:
    with urllib.request.urlopen(
        f"http://127.0.0.1:{port}/json/list", timeout=3
    ) as response:
        targets = json.load(response)
    page = next(
        (
            target
            for target in targets
            if target.get("type") == "page"
            and isinstance(target.get("webSocketDebuggerUrl"), str)
        ),
        None,
    )
    if page is None:
        raise ProbeError("Chromium exposed no debuggable noVNC page")
    debugger_url = urllib.parse.urlsplit(page["webSocketDebuggerUrl"])
    if debugger_url.hostname != "127.0.0.1" or debugger_url.port != port:
        raise ProbeError(f"unexpected Chromium debugger URL {debugger_url.geturl()}")
    websocket = WebSocket(
        debugger_url.hostname,
        debugger_url.port,
        debugger_url.path,
        protocol=None,
        origin=None,
    )
    try:
        devtools_command(websocket, 1, "Runtime.enable")
        devtools_command(websocket, 2, "Page.enable")
        command_id = 3
        deadline = time.monotonic() + 20
        browser_state: dict[str, object] | None = None
        while time.monotonic() < deadline:
            result = devtools_command(
                websocket,
                command_id,
                "Runtime.evaluate",
                {
                    "expression": (
                        "JSON.stringify({connected:document.documentElement.dataset.connected,"
                        "desktopName:document.documentElement.dataset.desktopName||'',"
                        "framebuffer:document.documentElement.dataset.framebuffer||''})"
                    ),
                    "returnByValue": True,
                },
            )
            command_id += 1
            remote = result.get("result", {})
            if isinstance(remote, dict) and isinstance(remote.get("value"), str):
                parsed = json.loads(remote["value"])
                if isinstance(parsed, dict):
                    browser_state = parsed
            if (
                browser_state is not None
                and browser_state.get("connected") == "true"
                and str(browser_state.get("desktopName", "")).startswith("xenoteer@")
                and browser_state.get("framebuffer") == "800x600"
            ):
                break
            time.sleep(0.1)
        else:
            raise ProbeError(f"noVNC browser state did not become ready: {browser_state}")

        capture = devtools_command(
            websocket, command_id, "Page.captureScreenshot", {"format": "png"}
        )
        encoded = capture.get("data")
        if not isinstance(encoded, str):
            raise ProbeError("Chromium screenshot response contained no data")
        screenshot = base64.b64decode(encoded, validate=True)
        if len(screenshot) < 24 or screenshot[:8] != b"\x89PNG\r\n\x1a\n":
            raise ProbeError("Chromium noVNC capture is not a PNG")
        screenshot_width, screenshot_height = struct.unpack(">II", screenshot[16:24])
        if screenshot_width < 800 or screenshot_height < 600 or len(screenshot) < 4096:
            raise ProbeError(
                "Chromium noVNC capture is implausible: "
                f"{screenshot_width}x{screenshot_height}, {len(screenshot)} bytes"
            )
        with open(screenshot_path, "wb") as output:
            output.write(screenshot)
        return {
            "actual_novnc_browser": True,
            "desktop_name": browser_state["desktopName"],
            "framebuffer": browser_state["framebuffer"],
            "screenshot_bytes": len(screenshot),
            "screenshot_geometry": f"{screenshot_width}x{screenshot_height}",
            "screenshot_sha256": hashlib.sha256(screenshot).hexdigest(),
        }
    finally:
        websocket.close()


def run_rfb_probe() -> dict[str, object]:
    websocket = WebSocket("127.0.0.1", 6080, "/websockify")
    try:
        version = websocket.recv_rfb(12)
        if not version.startswith(b"RFB 003."):
            raise ProbeError(f"invalid RFB banner: {version!r}")
        websocket.send_binary(b"RFB 003.008\n")
        security_count = websocket.recv_rfb(1)[0]
        if security_count == 0:
            reason_length = struct.unpack(">I", websocket.recv_rfb(4))[0]
            if reason_length > MAX_RFB_NAME:
                raise ProbeError("RFB rejection reason exceeds the configured bound")
            reason = websocket.recv_rfb(reason_length).decode("utf-8", "replace")
            raise ProbeError(f"RFB server rejected negotiation: {reason}")
        security_types = websocket.recv_rfb(security_count)
        if 1 not in security_types:
            raise ProbeError(f"RFB None security unavailable on loopback: {security_types!r}")
        websocket.send_binary(b"\x01")
        security_result = struct.unpack(">I", websocket.recv_rfb(4))[0]
        if security_result != 0:
            raise ProbeError(f"RFB security result was {security_result}")
        websocket.send_binary(b"\x01")
        width, height = struct.unpack(">HH", websocket.recv_rfb(4))
        pixel_format = websocket.recv_rfb(16)
        name_length = struct.unpack(">I", websocket.recv_rfb(4))[0]
        if name_length > MAX_RFB_NAME:
            raise ProbeError("RFB desktop name exceeds the configured bound")
        name = websocket.recv_rfb(name_length).decode("utf-8", "replace")
        if (width, height) != (800, 600):
            raise ProbeError(f"RFB geometry was {width}x{height}, expected Xvfb 800x600")
        bytes_per_pixel = pixel_format[0] // 8
        if bytes_per_pixel not in (2, 3, 4):
            raise ProbeError(f"unsupported RFB bits-per-pixel {pixel_format[0]}")

        websocket.send_binary(struct.pack(">BBHi", 2, 0, 1, 0))
        websocket.send_binary(struct.pack(">BBHHHH", 3, 0, 0, 0, 80, 80))
        pixels = bytearray()
        while not pixels:
            message_type = websocket.recv_rfb(1)[0]
            if message_type == 0:
                rectangle_count = struct.unpack(">xH", websocket.recv_rfb(3))[0]
                if rectangle_count > MAX_RFB_RECTANGLES:
                    raise ProbeError("RFB rectangle count exceeds the configured bound")
                for _ in range(rectangle_count):
                    _x, _y, rect_width, rect_height, encoding = struct.unpack(
                        ">HHHHi", websocket.recv_rfb(12)
                    )
                    if encoding != 0:
                        raise ProbeError(f"unexpected RFB encoding {encoding}")
                    if rect_width > width or rect_height > height:
                        raise ProbeError("RFB rectangle exceeds negotiated framebuffer geometry")
                    rectangle_bytes = rect_width * rect_height * bytes_per_pixel
                    if rectangle_bytes > MAX_FRAMEBUFFER_BYTES:
                        raise ProbeError("RFB rectangle exceeds the framebuffer byte bound")
                    if len(pixels) + rectangle_bytes > MAX_FRAMEBUFFER_BYTES:
                        raise ProbeError("RFB update exceeds the total framebuffer byte bound")
                    pixels.extend(websocket.recv_rfb(rectangle_bytes))
            elif message_type == 2:
                continue
            elif message_type == 3:
                cut_length = struct.unpack(">xxxI", websocket.recv_rfb(7))[0]
                if cut_length > MAX_RFB_TEXT:
                    raise ProbeError("RFB ServerCutText exceeds the configured bound")
                websocket.recv_rfb(cut_length)
            else:
                raise ProbeError(f"unexpected RFB server message {message_type}")
        chunks = {
            bytes(pixels[index : index + bytes_per_pixel])
            for index in range(0, len(pixels), bytes_per_pixel)
        }
        if len(chunks) < 2:
            raise ProbeError("framebuffer proof did not contain the recorder's black/white grid")

        websocket.send_binary(struct.pack(">BBxxI", 4, 1, 0x61))
        websocket.send_binary(struct.pack(">BBxxI", 4, 0, 0x61))
        websocket.send_binary(struct.pack(">BBHH", 5, 0, 120, 100))
        websocket.send_binary(struct.pack(">BBHH", 5, 1, 120, 100))
        websocket.send_binary(struct.pack(">BBHH", 5, 0, 120, 100))
        cut_text = b"rfb-viewer-must-not-own-x11-clipboard"
        websocket.send_binary(struct.pack(">BxxxI", 6, len(cut_text)) + cut_text)
        return {
            "framebuffer_bytes": len(pixels),
            "framebuffer_sha256": hashlib.sha256(pixels).hexdigest(),
            "geometry": f"{width}x{height}",
            "rfb_name": name,
            "rfb_version": version.decode("ascii").strip(),
            "sent_input_attempts": ["key", "pointer", "client_cut_text"],
            "websocket_subprotocol": "binary",
        }
    finally:
        websocket.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    wait_parser = subparsers.add_parser("wait-port")
    wait_parser.add_argument("host")
    wait_parser.add_argument("port", type=int)
    wait_parser.add_argument("--timeout", type=float, default=10.0)
    loopback_parser = subparsers.add_parser("assert-loopback")
    loopback_parser.add_argument("ports", nargs="+", type=int)
    png_parser = subparsers.add_parser("assert-png")
    png_parser.add_argument("path")
    png_parser.add_argument("width", type=int)
    png_parser.add_argument("height", type=int)
    chromium_parser = subparsers.add_parser("chromium")
    chromium_parser.add_argument("port", type=int)
    chromium_parser.add_argument("screenshot")
    subparsers.add_parser("rfb")
    arguments = parser.parse_args()
    if arguments.command == "wait-port":
        wait_port(arguments.host, arguments.port, arguments.timeout)
    elif arguments.command == "assert-loopback":
        assert_loopback(arguments.ports)
    elif arguments.command == "assert-png":
        assert_png(arguments.path, arguments.width, arguments.height)
    elif arguments.command == "chromium":
        print(
            json.dumps(
                run_chromium_probe(arguments.port, arguments.screenshot), sort_keys=True
            )
        )
    else:
        print(json.dumps(run_rfb_probe(), sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ProbeError, ValueError) as error:
        print(f"noVNC spike probe failed: {error}", file=sys.stderr)
        sys.exit(1)
