#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Exercise real RFC 6455 and RFB 3.8 bytes against the spike services."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
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
MAX_API_RESPONSE_BYTES = 1024 * 1024
MIN_VIEWER_TICKET_BYTES = 43
MAX_VIEWER_TICKET_BYTES = 128
VIEWER_BINARY_PROTOCOL = "binary"
VIEWER_TICKET_PROTOCOL_PREFIX = "xenoteer.ticket."


class ProbeError(RuntimeError):
    """A protocol or evidence assertion failed."""


def _extend_bounded(buffer: bytearray, payload: bytes, description: str) -> None:
    if len(buffer) > MAX_WEBSOCKET_PAYLOAD or len(payload) > (
        MAX_WEBSOCKET_PAYLOAD - len(buffer)
    ):
        raise ProbeError(
            f"{description} exceeds {MAX_WEBSOCKET_PAYLOAD} bytes"
        )
    buffer.extend(payload)


class WebSocket:
    def __init__(
        self,
        host: str,
        port: int,
        path: str,
        protocol: str | None = VIEWER_BINARY_PROTOCOL,
        origin: str | None = "http://localhost",
        requested_protocols: tuple[str, ...] | None = None,
    ) -> None:
        self._socket = socket.create_connection((host, port), timeout=5.0)
        self._socket.settimeout(5.0)
        self._network_buffer = bytearray()
        self._rfb_buffer = bytearray()
        self._upgrade(host, port, path, protocol, origin, requested_protocols)

    def _upgrade(
        self,
        host: str,
        port: int,
        path: str,
        protocol: str | None,
        origin: str | None,
        requested_protocols: tuple[str, ...] | None,
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
        protocols = requested_protocols
        if protocols is None:
            protocols = () if protocol is None else (protocol,)
        if protocols:
            request_lines.append(
                f"Sec-WebSocket-Protocol: {', '.join(protocols)}"
            )
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
                fragments = bytearray()
                _extend_bounded(fragments, payload, "fragmented WebSocket message")
                while not final:
                    final, continuation, payload = self._recv_frame()
                    if continuation != 0x0:
                        raise ProbeError("invalid fragmented WebSocket message")
                    _extend_bounded(
                        fragments, payload, "fragmented WebSocket message"
                    )
                payload = bytes(fragments)
            return opcode, payload

    def recv_rfb(self, length: int) -> bytes:
        if length < 0 or length > MAX_WEBSOCKET_PAYLOAD:
            raise ProbeError(f"refusing bounded RFB read of {length} bytes")
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
                fragments = bytearray()
                _extend_bounded(fragments, payload, "fragmented RFB message")
                while not final:
                    final, continuation, payload = self._recv_frame()
                    if continuation != 0x0:
                        raise ProbeError("invalid fragmented binary message")
                    _extend_bounded(fragments, payload, "fragmented RFB message")
                payload = bytes(fragments)
            _extend_bounded(self._rfb_buffer, payload, "RFB receive buffer")
        value = bytes(self._rfb_buffer[:length])
        del self._rfb_buffer[:length]
        return value

    def close(self) -> None:
        try:
            self._send_frame(0x8, struct.pack(">H", 1000))
        finally:
            self._socket.close()

    def set_timeout(self, timeout: float) -> None:
        self._socket.settimeout(timeout)


def viewer_protocols(ticket_file: str | None) -> tuple[str, ...]:
    """Build viewer protocols without accepting ticket material on argv."""
    if ticket_file is None:
        return (VIEWER_BINARY_PROTOCOL,)
    ticket_bytes = pathlib.Path(ticket_file).read_bytes()
    if ticket_bytes.endswith(b"\n"):
        ticket_bytes = ticket_bytes[:-1]
    if (
        len(ticket_bytes) < MIN_VIEWER_TICKET_BYTES
        or len(ticket_bytes) > MAX_VIEWER_TICKET_BYTES
        or any(
            value
            not in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
            for value in ticket_bytes
        )
    ):
        raise ProbeError("viewer ticket file is invalid")
    ticket = ticket_bytes.decode("ascii")
    return (
        VIEWER_BINARY_PROTOCOL,
        f"{VIEWER_TICKET_PROTOCOL_PREFIX}{ticket}",
    )


def _authenticated_api_json(
    host: str,
    port: int,
    path: str,
    origin: str,
    authorization: str,
    body: dict[str, object] | None = None,
    *,
    expected_status: int,
) -> dict[str, object]:
    data = None
    headers = {
        "Accept": "application/json",
        "Authorization": authorization,
        "Origin": origin,
    }
    if body is not None:
        data = json.dumps(body, separators=(",", ":")).encode("ascii")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        f"http://{host}:{port}{path}",
        data=data,
        headers=headers,
        method="POST" if data is not None else "GET",
    )
    try:
        with urllib.request.urlopen(request, timeout=5.0) as response:
            if response.status != expected_status:
                raise ProbeError("authenticated viewer API returned an unexpected status")
            payload = response.read(MAX_API_RESPONSE_BYTES + 1)
    except (OSError, ValueError) as error:
        raise ProbeError("authenticated viewer API request failed") from error
    if len(payload) > MAX_API_RESPONSE_BYTES:
        raise ProbeError("authenticated viewer API response exceeded its bound")
    try:
        parsed = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProbeError("authenticated viewer API returned invalid JSON") from error
    if not isinstance(parsed, dict):
        raise ProbeError("authenticated viewer API returned an invalid document")
    return parsed


def _write_private_viewer_files(
    files: tuple[tuple[pathlib.Path, bytes], ...],
) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptors: list[int] = []
    created_paths: list[pathlib.Path] = []
    try:
        for path, _ in files:
            descriptor = os.open(path, flags, 0o600)
            descriptors.append(descriptor)
            created_paths.append(path)
            os.fchmod(descriptor, 0o600)
        for index, ((_, contents), descriptor) in enumerate(zip(files, descriptors)):
            output = os.fdopen(descriptor, "wb")
            descriptors[index] = -1
            with output:
                output.write(contents)
    except (OSError, ValueError) as error:
        for descriptor in descriptors:
            if descriptor >= 0:
                try:
                    os.close(descriptor)
                except OSError:
                    pass
        for path in created_paths:
            try:
                path.unlink(missing_ok=True)
            except OSError:
                pass
        raise ProbeError("could not create private viewer evidence files") from error


def mint_viewer_ticket(
    host: str,
    port: int,
    origin: str,
    api_token_file: str,
    ticket_file: str,
    metadata_file: str,
) -> dict[str, object]:
    """Mint one real ticket while keeping both bearer values out of argv/output."""
    api_token = pathlib.Path(api_token_file).read_bytes()
    if (
        len(api_token) < 32
        or len(api_token) > 1026
        or any(value < 0x21 or value > 0x7E for value in api_token)
    ):
        raise ProbeError("API token file is invalid")
    authorization = f"Bearer {api_token.decode('ascii')}"
    status = _authenticated_api_json(
        host,
        port,
        "/v1/status",
        origin,
        authorization,
        expected_status=200,
    )
    desktop = status.get("desktop")
    if not isinstance(desktop, dict) or desktop.get("state") != "ready":
        raise ProbeError("authenticated viewer API desktop is not ready")
    desktop_id = desktop.get("id")
    desktop_generation = desktop.get("generation")
    if not isinstance(desktop_id, str) or not isinstance(desktop_generation, str):
        raise ProbeError("authenticated viewer API desktop identity is invalid")
    request_body = {
        "desktop_id": desktop_id,
        "desktop_generation": desktop_generation,
        "mode": "view_only",
    }
    ticket = _authenticated_api_json(
        host,
        port,
        f"/v1/desktops/{desktop_id}/viewer-tickets",
        origin,
        authorization,
        request_body,
        expected_status=201,
    )
    ticket_secret = ticket.get("ticket")
    if (
        not isinstance(ticket_secret, str)
        or ticket.get("desktop_id") != desktop_id
        or ticket.get("desktop_generation") != desktop_generation
        or ticket.get("origin") != origin
        or ticket.get("audience") != "viewer_websocket"
        or ticket.get("mode") != "view_only"
        or ticket.get("use_policy") != "single_use"
    ):
        raise ProbeError("authenticated viewer API returned invalid ticket claims")
    try:
        ticket_secret_bytes = ticket_secret.encode("ascii")
    except UnicodeEncodeError as error:
        raise ProbeError("authenticated viewer API returned an invalid ticket") from error
    if (
        len(ticket_secret_bytes) < MIN_VIEWER_TICKET_BYTES
        or len(ticket_secret_bytes) > MAX_VIEWER_TICKET_BYTES
        or any(
            value
            not in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
            for value in ticket_secret_bytes
        )
    ):
        raise ProbeError("authenticated viewer API returned an invalid ticket")
    gateway_path = (
        f"/v1/desktops/{desktop_id}/generations/"
        f"{desktop_generation}/viewer/ws"
    )
    metadata_bytes = (
        json.dumps(
            {
                "desktop_generation": desktop_generation,
                "desktop_id": desktop_id,
                "gateway_path": gateway_path,
            },
            sort_keys=True,
        )
        + "\n"
    ).encode("ascii")
    _write_private_viewer_files(
        (
            (pathlib.Path(ticket_file), ticket_secret_bytes + b"\n"),
            (pathlib.Path(metadata_file), metadata_bytes),
        )
    )
    return {
        "authenticated_api": True,
        "ticket_minted": True,
    }


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


def run_rfb_probe(
    expected_width: int = 800,
    expected_height: int = 600,
    prove_framebuffer: bool = True,
    ready_file: str | None = None,
    continue_file: str | None = None,
    observe_seconds: float = 0.0,
    forbidden_server_bytes_file: str | None = None,
    resize_ready_file: str | None = None,
    resize_continue_file: str | None = None,
    websocket_host: str = "127.0.0.1",
    websocket_port: int = 6080,
    websocket_path: str = "/websockify",
    websocket_origin: str = "http://localhost",
    gateway_ticket_file: str | None = None,
) -> dict[str, object]:
    if observe_seconds < 0:
        raise ProbeError("RFB observation duration cannot be negative")
    if (ready_file is None) != (continue_file is None):
        raise ProbeError("--ready-file and --continue-file must be used together")
    if (resize_ready_file is None) != (resize_continue_file is None):
        raise ProbeError(
            "--resize-ready-file and --resize-continue-file must be used together"
        )
    forbidden_server_bytes = b""
    if forbidden_server_bytes_file is not None:
        forbidden_server_bytes = pathlib.Path(forbidden_server_bytes_file).read_bytes()
        if not forbidden_server_bytes or len(forbidden_server_bytes) > MAX_RFB_TEXT:
            raise ProbeError("forbidden server byte canary has an invalid length")
    protocols = viewer_protocols(gateway_ticket_file)
    websocket = WebSocket(
        websocket_host,
        websocket_port,
        websocket_path,
        protocol=VIEWER_BINARY_PROTOCOL,
        origin=websocket_origin,
        requested_protocols=protocols,
    )
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
        if (width, height) != (expected_width, expected_height):
            raise ProbeError(
                f"RFB geometry was {width}x{height}, expected Xvfb "
                f"{expected_width}x{expected_height}"
            )
        bytes_per_pixel = pixel_format[0] // 8
        if bytes_per_pixel not in (2, 3, 4):
            raise ProbeError(f"unsupported RFB bits-per-pixel {pixel_format[0]}")

        pixels = bytearray()
        if prove_framebuffer:
            websocket.send_binary(struct.pack(">BBHi", 2, 0, 1, 0))
            websocket.send_binary(struct.pack(">BBHHHH", 3, 0, 0, 0, 80, 80))
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
                            raise ProbeError(
                                "RFB rectangle exceeds negotiated framebuffer geometry"
                            )
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
                raise ProbeError(
                    "framebuffer proof did not contain the recorder's black/white grid"
                )

        if ready_file is not None:
            if continue_file is None:
                raise ProbeError("--ready-file requires --continue-file")
            ready_path = pathlib.Path(ready_file)
            continue_path = pathlib.Path(continue_file)
            ready_path.write_text("rfb_client_ready\n", encoding="ascii")
            deadline = time.monotonic() + 10.0
            while not continue_path.exists():
                if time.monotonic() >= deadline:
                    raise ProbeError("RFB probe continuation barrier timed out")
                time.sleep(0.01)

        # Observe after the test sentinel acquires CLIPBOARD/PRIMARY but before
        # hostile client messages. A broken SendCutText policy is rejected
        # before exercising the separately ordered resize-denial path.
        if observe_seconds > 0:
            deadline = time.monotonic() + observe_seconds
            websocket.set_timeout(min(0.1, observe_seconds))
            while time.monotonic() < deadline:
                try:
                    message_type = websocket.recv_rfb(1)[0]
                except TimeoutError:
                    continue
                if message_type == 2:
                    continue
                if message_type == 3:
                    cut_length = struct.unpack(">xxxI", websocket.recv_rfb(7))[0]
                    if cut_length > MAX_RFB_TEXT:
                        raise ProbeError("RFB ServerCutText exceeds the configured bound")
                    cut_payload = websocket.recv_rfb(cut_length)
                    if forbidden_server_bytes and forbidden_server_bytes in cut_payload:
                        raise ProbeError(
                            "viewer received forbidden clipboard canary in ServerCutText"
                        )
                    raise ProbeError("viewer received forbidden RFB ServerCutText")
                raise ProbeError(
                    f"unexpected RFB server message during clipboard denial {message_type}"
                )

        # Advertise ExtendedDesktopSize before the hostile request. TigerVNC
        # then returns an ordered reason=client/result=prohibited rectangle
        # instead of closing a client that cannot receive its resize result.
        websocket.send_binary(struct.pack(">BBHii", 2, 0, 2, 0, -308))
        websocket.send_binary(struct.pack(">BBxxI", 4, 1, 0x61))
        websocket.send_binary(struct.pack(">BBxxI", 4, 0, 0x61))
        websocket.send_binary(struct.pack(">BBHH", 5, 0, 120, 100))
        websocket.send_binary(struct.pack(">BBHH", 5, 1, 120, 100))
        websocket.send_binary(struct.pack(">BBHH", 5, 0, 120, 100))
        cut_text = b"rfb-viewer-must-not-own-x11-clipboard"
        websocket.send_binary(struct.pack(">BxxxI", 6, len(cut_text)) + cut_text)
        websocket.send_binary(
            struct.pack(">BBHHBBIHHHHI", 251, 0, 1024, 768, 1, 0, 0, 0, 0, 1024, 768, 0)
        )
        # Client messages are processed in transport order. This
        # non-incremental request makes the queued resize rejection observable
        # as a FramebufferUpdate ExtendedDesktopSize pseudo-rectangle.
        websocket.send_binary(struct.pack(">BBHHHH", 3, 0, 0, 0, 1, 1))
        websocket.set_timeout(5.0)
        resize_rejection: dict[str, object] | None = None
        for _ in range(32):
            message_type = websocket.recv_rfb(1)[0]
            if message_type == 2:
                continue
            if message_type == 3:
                cut_length = struct.unpack(">xxxI", websocket.recv_rfb(7))[0]
                if cut_length > MAX_RFB_TEXT:
                    raise ProbeError("RFB ServerCutText exceeds the configured bound")
                cut_payload = websocket.recv_rfb(cut_length)
                if forbidden_server_bytes and forbidden_server_bytes in cut_payload:
                    raise ProbeError(
                        "viewer received forbidden clipboard canary in ServerCutText"
                    )
                raise ProbeError("viewer received forbidden RFB ServerCutText")
            if message_type != 0:
                raise ProbeError(
                    f"unexpected RFB server message during resize barrier {message_type}"
                )
            rectangle_count = struct.unpack(">xH", websocket.recv_rfb(3))[0]
            if rectangle_count > MAX_RFB_RECTANGLES:
                raise ProbeError("RFB rectangle count exceeds the configured bound")
            for _ in range(rectangle_count):
                rect_x, rect_y, rect_width, rect_height, encoding = struct.unpack(
                    ">HHHHi", websocket.recv_rfb(12)
                )
                if encoding == 0:
                    rectangle_bytes = rect_width * rect_height * bytes_per_pixel
                    if rectangle_bytes > MAX_FRAMEBUFFER_BYTES:
                        raise ProbeError("RFB rectangle exceeds the framebuffer byte bound")
                    websocket.recv_rfb(rectangle_bytes)
                    continue
                if encoding != -308:
                    raise ProbeError(
                        f"unexpected RFB encoding during resize barrier {encoding}"
                    )
                screen_count = websocket.recv_rfb(1)[0]
                websocket.recv_rfb(3)
                if screen_count > 64:
                    raise ProbeError("ExtendedDesktopSize screen count exceeds bound")
                screens: list[dict[str, int]] = []
                for _ in range(screen_count):
                    screen_id, screen_x, screen_y, screen_width, screen_height, flags = (
                        struct.unpack(">IHHHHI", websocket.recv_rfb(16))
                    )
                    screens.append(
                        {
                            "id": screen_id,
                            "x": screen_x,
                            "y": screen_y,
                            "width": screen_width,
                            "height": screen_height,
                            "flags": flags,
                        }
                    )
                if rect_x != 1:
                    continue
                if rect_y != 1:
                    raise ProbeError(
                        f"SetDesktopSize result was {rect_y}, expected prohibited (1)"
                    )
                if (rect_width, rect_height) != (width, height):
                    raise ProbeError(
                        "resize rejection reported changed RFB geometry "
                        f"{rect_width}x{rect_height}, ServerInit was {width}x{height}"
                    )
                if not any(
                    screen["x"] == 0
                    and screen["y"] == 0
                    and screen["width"] == width
                    and screen["height"] == height
                    for screen in screens
                ):
                    raise ProbeError(
                        "resize rejection layout does not cover unchanged ServerInit geometry"
                    )
                resize_rejection = {
                    "ordered_protocol_barrier": "extended_desktop_size",
                    "reason": "client",
                    "reason_code": rect_x,
                    "result": "prohibited",
                    "result_code": rect_y,
                    "requested_geometry": "1024x768",
                    "server_init_geometry": f"{width}x{height}",
                    "response_geometry": f"{rect_width}x{rect_height}",
                    "screens": screens,
                }
                break
            if resize_rejection is not None:
                break
        if resize_rejection is None:
            raise ProbeError("timed out without ordered ExtendedDesktopSize rejection")

        if resize_ready_file is not None:
            if resize_continue_file is None:
                raise ProbeError("--resize-ready-file requires --resize-continue-file")
            pathlib.Path(resize_ready_file).write_text(
                json.dumps(resize_rejection, sort_keys=True) + "\n", encoding="ascii"
            )
            resize_continue_path = pathlib.Path(resize_continue_file)
            deadline = time.monotonic() + 30.0
            while not resize_continue_path.exists():
                if time.monotonic() >= deadline:
                    raise ProbeError("RFB resize evidence continuation barrier timed out")
                time.sleep(0.01)
        return {
            "framebuffer_bytes": len(pixels),
            "framebuffer_sha256": hashlib.sha256(pixels).hexdigest(),
            "geometry": f"{width}x{height}",
            "rfb_name": name,
            "rfb_version": version.decode("ascii").strip(),
            "sent_input_attempts": [
                "key",
                "pointer",
                "client_cut_text",
                "set_desktop_size",
            ],
            "resize_rejection": resize_rejection,
            "server_cut_text_observation_seconds": observe_seconds,
            "server_cut_text_messages": 0,
            "forbidden_server_bytes_seen": False,
            "forbidden_server_bytes_sha256": hashlib.sha256(
                forbidden_server_bytes
            ).hexdigest(),
            "gateway_authenticated": gateway_ticket_file is not None,
            "websocket_subprotocol": VIEWER_BINARY_PROTOCOL,
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
    mint_parser = subparsers.add_parser("mint-ticket")
    mint_parser.add_argument("--host", default="127.0.0.1")
    mint_parser.add_argument("--port", type=int, default=8080)
    mint_parser.add_argument("--origin", required=True)
    mint_parser.add_argument("--api-token-file", required=True)
    mint_parser.add_argument("--ticket-file", required=True)
    mint_parser.add_argument("--metadata-file", required=True)
    rfb_parser = subparsers.add_parser("rfb")
    rfb_parser.add_argument("--width", type=int, default=800)
    rfb_parser.add_argument("--height", type=int, default=600)
    rfb_parser.add_argument("--skip-framebuffer-proof", action="store_true")
    rfb_parser.add_argument("--ready-file")
    rfb_parser.add_argument("--continue-file")
    rfb_parser.add_argument("--observe-seconds", type=float, default=0.0)
    rfb_parser.add_argument("--forbidden-server-bytes-file")
    rfb_parser.add_argument("--resize-ready-file")
    rfb_parser.add_argument("--resize-continue-file")
    rfb_parser.add_argument("--host", default="127.0.0.1")
    rfb_parser.add_argument("--port", type=int, default=6080)
    rfb_parser.add_argument("--path", default="/websockify")
    rfb_parser.add_argument("--origin", default="http://localhost")
    rfb_parser.add_argument("--gateway-ticket-file")
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
    elif arguments.command == "mint-ticket":
        print(
            json.dumps(
                mint_viewer_ticket(
                    arguments.host,
                    arguments.port,
                    arguments.origin,
                    arguments.api_token_file,
                    arguments.ticket_file,
                    arguments.metadata_file,
                ),
                sort_keys=True,
            )
        )
    else:
        print(
            json.dumps(
                run_rfb_probe(
                    arguments.width,
                    arguments.height,
                    not arguments.skip_framebuffer_proof,
                    arguments.ready_file,
                    arguments.continue_file,
                    arguments.observe_seconds,
                    arguments.forbidden_server_bytes_file,
                    arguments.resize_ready_file,
                    arguments.resize_continue_file,
                    arguments.host,
                    arguments.port,
                    arguments.path,
                    arguments.origin,
                    arguments.gateway_ticket_file,
                ),
                sort_keys=True,
            )
        )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ProbeError, ValueError) as error:
        print(f"noVNC spike probe failed: {error}", file=sys.stderr)
        sys.exit(1)
