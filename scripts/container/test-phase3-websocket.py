#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Live Phase 3 WebSocket acceptance tests using only the Python standard library."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlsplit


MAGIC_GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_HTTP_HEADER_BYTES = 64 * 1024
MAX_SERVER_MESSAGE_BYTES = 2 * 1024 * 1024
EXPECTED_MESSAGE_LIMIT = 1024 * 1024


class AcceptanceFailure(RuntimeError):
    """A safe failure that never embeds request headers or credentials."""


@dataclass(frozen=True)
class Close:
    code: int | None
    reason: str


def require(condition: bool, detail: str) -> None:
    if not condition:
        raise AcceptanceFailure(detail)


def run_named_check(name: str, check: Any) -> None:
    try:
        check()
    except AcceptanceFailure as error:
        raise AcceptanceFailure(f"{name}: {error}") from error


def new_id() -> str:
    return str(uuid.uuid4())


class WebSocket:
    """Small strict RFC 6455 client sufficient for loopback acceptance tests."""

    def __init__(self, api_base: str, token: bytes, origin: str | None = None) -> None:
        parsed = urlsplit(api_base)
        require(parsed.scheme == "http", "WebSocket acceptance requires a local HTTP API URL")
        require(parsed.hostname in {"127.0.0.1", "localhost", "::1"}, "API URL is not loopback")
        require(parsed.username is None and parsed.password is None, "API URL contains credentials")
        require(parsed.query == "" and parsed.fragment == "", "API base URL contains a query or fragment")
        self._host = parsed.hostname or ""
        self._port = parsed.port or 80
        prefix = parsed.path.rstrip("/")
        self._path = f"{prefix}/v1/ws" if prefix else "/v1/ws"
        self._token = token
        self._origin = origin
        self._socket: socket.socket | None = None
        self._buffer = bytearray()
        self._fragments: bytearray | None = None
        self._fragment_opcode: int | None = None

    def connect(self, expected_status: int = 101) -> int:
        require(self._socket is None, "WebSocket client was connected twice")
        sock = socket.create_connection((self._host, self._port), timeout=5.0)
        sock.settimeout(5.0)
        key = base64.b64encode(os.urandom(16))
        host_header = self._host if self._port == 80 else f"{self._host}:{self._port}"
        headers = [
            f"GET {self._path} HTTP/1.1".encode("ascii"),
            f"Host: {host_header}".encode("ascii"),
            b"Upgrade: websocket",
            b"Connection: Upgrade",
            b"Sec-WebSocket-Version: 13",
            b"Sec-WebSocket-Key: " + key,
            b"Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits",
            b"Authorization: Bearer " + self._token,
        ]
        if self._origin is not None:
            headers.append(f"Origin: {self._origin}".encode("ascii"))
        sock.sendall(b"\r\n".join(headers) + b"\r\n\r\n")
        response = bytearray()
        while b"\r\n\r\n" not in response:
            chunk = sock.recv(4096)
            if not chunk:
                sock.close()
                raise AcceptanceFailure("WebSocket HTTP handshake ended before its headers")
            response.extend(chunk)
            require(len(response) <= MAX_HTTP_HEADER_BYTES, "WebSocket HTTP response headers were oversized")
        header_block, remainder = bytes(response).split(b"\r\n\r\n", 1)
        lines = header_block.split(b"\r\n")
        try:
            status = int(lines[0].split(b" ", 2)[1])
        except (IndexError, ValueError) as error:
            sock.close()
            raise AcceptanceFailure("WebSocket HTTP status line was malformed") from error
        require(status == expected_status, f"WebSocket HTTP handshake returned status {status}, expected {expected_status}")
        if status != 101:
            sock.close()
            return status

        response_headers: dict[bytes, list[bytes]] = {}
        for line in lines[1:]:
            name, separator, value = line.partition(b":")
            require(bool(separator), "WebSocket HTTP response contained a malformed header")
            response_headers.setdefault(name.strip().lower(), []).append(value.strip())
        expected_accept = base64.b64encode(hashlib.sha1(key + MAGIC_GUID).digest())
        require(response_headers.get(b"sec-websocket-accept") == [expected_accept], "WebSocket accept digest was invalid")
        require(response_headers.get(b"upgrade", [b""])[0].lower() == b"websocket", "WebSocket upgrade header was absent")
        require(b"upgrade" in response_headers.get(b"connection", [b""])[0].lower(), "WebSocket connection header was absent")
        require(
            b"sec-websocket-extensions" not in response_headers,
            "server negotiated WebSocket compression even though permessage-deflate must be disabled",
        )
        self._socket = sock
        self._buffer.extend(remainder)
        return status

    def close_transport(self) -> None:
        if self._socket is not None:
            self._socket.close()
            self._socket = None

    def send_json(self, value: dict[str, Any]) -> None:
        payload = json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        self.send_frame(0x1, payload)

    def send_frame(self, opcode: int, payload: bytes, *, final: bool = True) -> None:
        require(self._socket is not None, "WebSocket transport is not connected")
        require(0 <= opcode <= 0xF, "invalid test frame opcode")
        first = (0x80 if final else 0) | opcode
        length = len(payload)
        mask = os.urandom(4)
        if length < 126:
            header = bytes((first, 0x80 | length))
        elif length <= 0xFFFF:
            header = bytes((first, 0x80 | 126)) + struct.pack("!H", length)
        else:
            header = bytes((first, 0x80 | 127)) + struct.pack("!Q", length)
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self._socket.sendall(header + mask + masked)

    def recv(self, timeout: float = 8.0) -> dict[str, Any] | Close:
        deadline = time.monotonic() + timeout
        while True:
            opcode, final, payload = self._recv_frame(deadline)
            if opcode == 0x8:
                if not payload:
                    return Close(None, "")
                require(len(payload) >= 2, "server close frame had a one-byte payload")
                code = struct.unpack("!H", payload[:2])[0]
                try:
                    reason = payload[2:].decode("utf-8")
                except UnicodeDecodeError as error:
                    raise AcceptanceFailure("server close reason was not UTF-8") from error
                return Close(code, reason)
            if opcode == 0x9:
                self.send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode in {0x1, 0x2}:
                require(self._fragments is None, "server interleaved fragmented data messages")
                if final:
                    require(opcode == 0x1, "server emitted an unexpected binary application message")
                    return self._decode_json(payload)
                self._fragments = bytearray(payload)
                self._fragment_opcode = opcode
                continue
            if opcode == 0x0:
                require(self._fragments is not None, "server emitted an unexpected continuation frame")
                self._fragments.extend(payload)
                require(len(self._fragments) <= MAX_SERVER_MESSAGE_BYTES, "fragmented server message was oversized")
                if not final:
                    continue
                complete = bytes(self._fragments)
                original_opcode = self._fragment_opcode
                self._fragments = None
                self._fragment_opcode = None
                require(original_opcode == 0x1, "server emitted a fragmented binary application message")
                return self._decode_json(complete)
            raise AcceptanceFailure(f"server emitted reserved WebSocket opcode {opcode}")

    def _decode_json(self, payload: bytes) -> dict[str, Any]:
        require(len(payload) <= MAX_SERVER_MESSAGE_BYTES, "server application message was oversized")
        try:
            decoded = json.loads(payload.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise AcceptanceFailure("server application message was not valid UTF-8 JSON") from error
        require(isinstance(decoded, dict), "server application message was not a JSON object")
        return decoded

    def _recv_frame(self, deadline: float) -> tuple[int, bool, bytes]:
        header = self._read_exact(2, deadline)
        final = bool(header[0] & 0x80)
        require(header[0] & 0x70 == 0, "server frame used an unnegotiated RSV bit")
        opcode = header[0] & 0x0F
        require(header[1] & 0x80 == 0, "server frame was incorrectly masked")
        length = header[1] & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_exact(2, deadline))[0]
            require(length >= 126, "server frame used a non-minimal 16-bit length")
        elif length == 127:
            encoded = self._read_exact(8, deadline)
            require(encoded[0] & 0x80 == 0, "server frame length exceeded RFC 6455 range")
            length = struct.unpack("!Q", encoded)[0]
            require(length > 0xFFFF, "server frame used a non-minimal 64-bit length")
        if opcode >= 0x8:
            require(final and length <= 125, "server emitted an invalid control frame")
        require(length <= MAX_SERVER_MESSAGE_BYTES, "server frame exceeded the test receive bound")
        return opcode, final, self._read_exact(length, deadline)

    def _read_exact(self, count: int, deadline: float) -> bytes:
        require(self._socket is not None, "WebSocket transport is not connected")
        while len(self._buffer) < count:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AcceptanceFailure("timed out waiting for a WebSocket frame")
            self._socket.settimeout(remaining)
            try:
                chunk = self._socket.recv(min(65536, count - len(self._buffer)))
            except socket.timeout as error:
                raise AcceptanceFailure("timed out waiting for a WebSocket frame") from error
            if not chunk:
                raise AcceptanceFailure("WebSocket transport ended without a close frame")
            self._buffer.extend(chunk)
        result = bytes(self._buffer[:count])
        del self._buffer[:count]
        return result


def read_token(path: str) -> bytes:
    with open(path, "rb") as token_file:
        token = token_file.read(4097)
    require(32 <= len(token) <= 4096, "API token file has an invalid byte length")
    require(token == token.strip() and b"\x00" not in token, "API token file has invalid whitespace or NUL bytes")
    require(all(0x21 <= byte <= 0x7E for byte in token), "API token is not visible ASCII")
    return token


def read_process_ref(path: str, generation: str) -> dict[str, Any]:
    with open(path, "r", encoding="utf-8") as process_file:
        process = json.load(process_file)
    require(isinstance(process, dict), "process reference file was not a JSON object")
    require(
        set(process) == {"desktop_generation", "pid", "proc_start_ticks", "launch_id"},
        "process reference file had an unexpected shape",
    )
    require(process["desktop_generation"] == generation, "process reference used the wrong generation")
    for field in ("pid", "proc_start_ticks"):
        require(
            isinstance(process[field], int) and not isinstance(process[field], bool) and process[field] > 0,
            f"process reference {field} was invalid",
        )
    require(
        isinstance(process["launch_id"], str) and uuid.UUID(process["launch_id"]).int != 0,
        "process launch ID was invalid",
    )
    return process


def hello(client: WebSocket, desktop_id: str, generation: str, resume_sequence: int | None = None) -> dict[str, Any]:
    resume = None
    if resume_sequence is not None:
        resume = {
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "event_sequence": resume_sequence,
        }
    client.send_json(
        {
            "type": "client.hello",
            "request_id": new_id(),
            "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
            "client": {"name": "xenoteer-phase3-blackbox", "version": "1.0.0"},
            "resume": resume,
        }
    )
    message = client.recv()
    if isinstance(message, Close):
        observed = f"close code {message.code}"
    else:
        observed_type = message.get("type")
        observed = observed_type if isinstance(observed_type, str) else "untyped JSON"
    require(
        isinstance(message, dict) and message.get("type") == "server.welcome",
        f"server sent {observed} instead of welcome first",
    )
    require(set(message) == {"type", "protocol", "connection_id", "principal", "desktop", "limits", "resume"}, "welcome top-level shape changed")
    require(message["protocol"] == {"major": 1, "minor": 0}, "welcome negotiated the wrong protocol")
    require(isinstance(message["connection_id"], str) and uuid.UUID(message["connection_id"]).int != 0, "welcome connection ID was invalid")
    require(set(message["principal"]) == {"id", "capabilities"}, "welcome principal shape changed")
    require(isinstance(message["principal"]["id"], str) and message["principal"]["id"], "welcome principal ID was absent")
    require(isinstance(message["principal"]["capabilities"], list), "welcome capabilities were not a list")
    require(set(message["desktop"]) == {"id", "generation", "state"}, "welcome desktop shape changed")
    require(message["desktop"] == {"id": desktop_id, "generation": generation, "state": "ready"}, "welcome desktop identity/state differed from status")
    limits = message["limits"]
    require(set(limits) == {"max_message_bytes", "heartbeat_ms", "normal_outbound_capacity", "reserved_outbound_capacity", "max_command_watches"}, "welcome limits shape changed")
    require(limits["max_message_bytes"] == EXPECTED_MESSAGE_LIMIT, "welcome message-size limit changed")
    require(limits["heartbeat_ms"] > 0 and limits["normal_outbound_capacity"] > 0 and limits["reserved_outbound_capacity"] > 0 and limits["max_command_watches"] > 0, "welcome contained a zero transport limit")
    require(set(message["resume"]) == {"status"}, "welcome resume shape changed")
    return message


def expect_message(client: WebSocket, message_type: str, request_id: str | None = None, timeout: float = 8.0) -> dict[str, Any]:
    message = client.recv(timeout)
    require(isinstance(message, dict), f"received close while waiting for {message_type}")
    require(message.get("type") == message_type, f"received {message.get('type')} while waiting for {message_type}")
    if request_id is not None:
        require(message.get("request_id") == request_id, f"{message_type} request ID did not match")
    return message


def lease_message(message_type: str, request_id: str, desktop_id: str, generation: str, **fields: Any) -> dict[str, Any]:
    lease = {
        "protocol_version": {"major": 1, "minor": 0},
        "request_id": request_id,
        "desktop_id": desktop_id,
        "desktop_generation": generation,
    }
    lease.update(fields)
    return {"type": message_type, "request_id": request_id, "lease": lease}


def run_exercise(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    denied = WebSocket(api_base, token, origin="https://untrusted.invalid")
    denied.connect(expected_status=403)

    run_named_check("strict hello rejection", lambda: assert_strict_hello_close(api_base, token))

    client = WebSocket(api_base, token)
    client.connect()
    welcome = hello(client, desktop_id, generation)
    require(welcome["resume"] == {"status": "not_requested"}, "fresh connection did not report not_requested resume")

    ping_id = new_id()
    nonce = f"phase3-{new_id()}"
    client.send_json({"type": "client.ping", "request_id": ping_id, "nonce": nonce})
    pong = expect_message(client, "server.pong", ping_id)
    require(pong.get("nonce") == nonce, "application pong did not echo its nonce")

    subscribe_id = new_id()
    topics = ["command.lifecycle", "action.lifecycle"]
    client.send_json(
        {
            "type": "events.subscribe",
            "request_id": subscribe_id,
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "topics": topics,
            "since_sequence": None,
        }
    )
    subscribed = expect_message(client, "events.subscribed", subscribe_id)
    require(subscribed.get("topics") == topics, "event subscription topics changed")
    replay = expect_message(client, "events.replay_complete", subscribe_id)
    require(replay.get("desktop_id") == desktop_id and replay.get("desktop_generation") == generation, "replay boundary identity changed")
    require(isinstance(replay.get("through_sequence"), int), "replay boundary sequence was absent")

    acquire_id = new_id()
    client.send_json(lease_message("lease.acquire", acquire_id, desktop_id, generation, ttl_ms=30_000))
    lease_state = expect_message(client, "lease.state", acquire_id)["lease"]
    require(lease_state.get("state") == "held_by_caller", "WebSocket lease acquisition did not grant the caller")
    lease_id = lease_state.get("lease_id")
    require(isinstance(lease_id, str) and uuid.UUID(lease_id).int != 0, "WebSocket lease ID was invalid")

    get_id = new_id()
    client.send_json({"type": "lease.get", "request_id": get_id, "desktop_id": desktop_id, "desktop_generation": generation})
    require(expect_message(client, "lease.state", get_id)["lease"].get("lease_id") == lease_id, "WebSocket lease.get lost caller ownership")
    renew_id = new_id()
    client.send_json(lease_message("lease.renew", renew_id, desktop_id, generation, lease_id=lease_id, ttl_ms=30_000))
    require(expect_message(client, "lease.state", renew_id)["lease"].get("lease_id") == lease_id, "WebSocket lease.renew changed lease identity")

    command_id = new_id()
    command_request_id = new_id()
    client.send_json(
        {
            "type": "command.submit",
            "request_id": command_request_id,
            "command": {
                "protocol_version": {"major": 1, "minor": 0},
                "request_id": command_request_id,
                "command_id": command_id,
                "desktop_id": desktop_id,
                "desktop_generation": generation,
                "lease_id": lease_id,
                "deadline": None,
                "trace_policy": "detailed",
                "command": {"type": "pointer_move", "target": {"x": 320, "y": 240}, "duration_ms": 350, "curve": "smooth"},
            },
        }
    )
    sequences: list[int] = []
    lifecycle: list[tuple[str, str, str | None]] = []
    terminal_result = False
    terminal_event = False
    deadline = time.monotonic() + 12.0
    while not (terminal_result and terminal_event):
        message = client.recv(max(0.1, deadline - time.monotonic()))
        require(isinstance(message, dict), "connection closed during smooth-pointer lifecycle")
        message_type = message.get("type")
        if message_type == "event":
            require(message.get("request_id") == subscribe_id, "live event used the wrong subscription request ID")
            event = message.get("event")
            require(isinstance(event, dict), "event envelope was not nested")
            require(event.get("desktop_id") == desktop_id and event.get("desktop_generation") == generation, "event desktop identity changed")
            sequence = event.get("sequence")
            require(isinstance(sequence, int) and sequence > 0, "event sequence was invalid")
            require(not sequences or sequence > sequences[-1], "delivered event sequence was not globally monotonic")
            sequences.append(sequence)
            payload = event.get("payload")
            if isinstance(payload, dict) and payload.get("command_id") == command_id:
                topic = event.get("topic")
                lifecycle.append((str(topic), str(payload.get("command_lifecycle")), payload.get("action_state")))
                if topic == "command.lifecycle" and payload.get("command_lifecycle") == "terminal":
                    require(payload.get("action_state") == "completed" and isinstance(payload.get("terminal"), dict), "terminal lifecycle event lacked completion evidence")
                    terminal_event = True
        elif message_type in {"command.accepted", "command.progress", "command.result"}:
            require(message.get("request_id") == command_request_id, "command lifecycle response used the wrong request ID")
            result = message.get("result")
            require(isinstance(result, dict) and result.get("command_id") == command_id, "command lifecycle response used the wrong command ID")
            if message_type == "command.result":
                require(result.get("lifecycle") == "succeeded", "smooth pointer command did not succeed")
                require(result.get("effect_stage") == "pointer_moved", "smooth pointer command reported the wrong effect stage")
                require(result.get("outcome") == {"type": "acknowledged"}, "smooth pointer command reported the wrong outcome")
                terminal_result = True
        else:
            raise AcceptanceFailure(f"unexpected message {message_type} during command lifecycle")
    require(any(topic == "command.lifecycle" and state == "accepted" for topic, state, _ in lifecycle), "command admission event was absent")
    require(any(topic == "action.lifecycle" and action == "started" for topic, _, action in lifecycle), "action-start event was absent")
    relevant_lifecycle = [item for item in lifecycle if item[0] in topics]
    require(
        relevant_lifecycle
        == [
            ("command.lifecycle", "accepted", None),
            ("action.lifecycle", "running", "started"),
            ("command.lifecycle", "terminal", "completed"),
        ],
        "command/action lifecycle transitions were not delivered in order",
    )
    require(len(sequences) >= 3, "smooth pointer lifecycle did not produce the expected event sequence")

    unwatch_id = new_id()
    client.send_json({"type": "command.unwatch", "request_id": unwatch_id, "desktop_id": desktop_id, "desktop_generation": generation, "command_id": command_id})
    unwatched = expect_message(client, "command.unwatched", unwatch_id)
    require(unwatched.get("command_id") == command_id and unwatched.get("watching") is False, "command unwatch acknowledgment was invalid")

    unsubscribe_id = new_id()
    client.send_json({"type": "events.unsubscribe", "request_id": unsubscribe_id, "desktop_id": desktop_id, "desktop_generation": generation})
    expect_message(client, "events.unsubscribed", unsubscribe_id)

    resume_sequence = sequences[-1]
    # Deliberately lose the transport without releasing. Lease ownership is
    # principal/TTL-bound, not tied to a particular WebSocket session.
    client.close_transport()


    resumed = WebSocket(api_base, token)
    resumed.connect()
    resumed_welcome = hello(resumed, desktop_id, generation, resume_sequence)
    require(resumed_welcome["resume"] == {"status": "replayed"}, "retained resume was not accepted")
    seen_sequence = resume_sequence
    while True:
        message = resumed.recv()
        require(isinstance(message, dict), "resumed connection closed before replay boundary")
        if message.get("type") == "events.replay_complete":
            require(message.get("through_sequence", -1) >= seen_sequence, "resume replay boundary moved backwards")
            break
        require(message.get("type") == "event", "resume emitted a message before replay completion")
        event = message.get("event", {})
        sequence = event.get("sequence")
        require(isinstance(sequence, int) and sequence > seen_sequence, "resumed event sequence was not monotonic")
        seen_sequence = sequence

    resumed_unsubscribe_id = new_id()
    resumed.send_json(
        {
            "type": "events.unsubscribe",
            "request_id": resumed_unsubscribe_id,
            "desktop_id": desktop_id,
            "desktop_generation": generation,
        }
    )
    expect_message(resumed, "events.unsubscribed", resumed_unsubscribe_id)

    resumed_get_id = new_id()
    resumed.send_json(
        {
            "type": "lease.get",
            "request_id": resumed_get_id,
            "desktop_id": desktop_id,
            "desktop_generation": generation,
        }
    )
    resumed_lease = expect_message(resumed, "lease.state", resumed_get_id)["lease"]
    require(
        resumed_lease.get("state") == "held_by_caller" and resumed_lease.get("lease_id") == lease_id,
        "same-principal reconnect did not retain the original lease",
    )

    resumed_renew_id = new_id()
    resumed.send_json(
        lease_message(
            "lease.renew",
            resumed_renew_id,
            desktop_id,
            generation,
            lease_id=lease_id,
            ttl_ms=30_000,
        )
    )
    renewed_lease = expect_message(resumed, "lease.state", resumed_renew_id)["lease"]
    require(
        renewed_lease.get("state") == "held_by_caller" and renewed_lease.get("lease_id") == lease_id,
        "same-principal reconnect could not renew the original lease",
    )

    resumed_command_id = new_id()
    resumed_command_request_id = new_id()
    resumed.send_json(
        {
            "type": "command.submit",
            "request_id": resumed_command_request_id,
            "command": {
                "protocol_version": {"major": 1, "minor": 0},
                "request_id": resumed_command_request_id,
                "command_id": resumed_command_id,
                "desktop_id": desktop_id,
                "desktop_generation": generation,
                "lease_id": lease_id,
                "deadline": None,
                "trace_policy": "normal",
                "command": {
                    "type": "pointer_move",
                    "target": {"x": 321, "y": 241},
                    "duration_ms": 80,
                    "curve": "smooth",
                },
            },
        }
    )
    while True:
        message = resumed.recv()
        require(isinstance(message, dict), "reconnected transport closed while using retained lease")
        require(
            message.get("type") in {"command.accepted", "command.progress", "command.result"},
            "retained-lease command produced an unexpected message",
        )
        require(message.get("request_id") == resumed_command_request_id, "retained-lease command request ID changed")
        result = message.get("result")
        require(
            isinstance(result, dict) and result.get("command_id") == resumed_command_id,
            "retained-lease command ID changed",
        )
        if message.get("type") == "command.result":
            require(result.get("lifecycle") == "succeeded", "retained lease could not execute input after reconnect")
            require(result.get("effect_stage") == "pointer_moved", "retained-lease input reported the wrong effect")
            break

    resumed_unwatch_id = new_id()
    resumed.send_json(
        {
            "type": "command.unwatch",
            "request_id": resumed_unwatch_id,
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "command_id": resumed_command_id,
        }
    )
    expect_message(resumed, "command.unwatched", resumed_unwatch_id)

    release_id = new_id()
    resumed.send_json(lease_message("lease.release", release_id, desktop_id, generation, lease_id=lease_id))
    released = expect_message(resumed, "lease.state", release_id)["lease"]
    require(
        released.get("state") == "vacant" and released.get("lease_id") is None,
        "same-principal reconnect could not explicitly release the original lease",
    )
    resumed.send_frame(0x8, struct.pack("!H", 1000))
    resumed.close_transport()

    ahead = WebSocket(api_base, token)
    ahead.connect()
    ahead_welcome = hello(ahead, desktop_id, generation, (1 << 63) - 1)
    require(ahead_welcome["resume"] == {"status": "resync_required"}, "unsafe resume cursor was not rejected in welcome")
    resync = expect_message(ahead, "events.resync_required")
    require(resync.get("reason") == "sequence_ahead", "unsafe resume returned the wrong resync reason")
    require(resync.get("desktop_id") == desktop_id and resync.get("desktop_generation") == generation, "resync identity changed")
    ahead.send_frame(0x8, struct.pack("!H", 1000))
    ahead.close_transport()

    run_named_check(
        "malformed JSON close",
        lambda: assert_invalid_message_close(api_base, token, desktop_id, generation),
    )
    run_named_check(
        "invalid UTF-8 close",
        lambda: assert_invalid_utf8_close(api_base, token, desktop_id, generation),
    )
    run_named_check(
        "binary message close",
        lambda: assert_binary_message_close(api_base, token, desktop_id, generation),
    )
    run_named_check(
        "oversized message close",
        lambda: assert_oversize_close(api_base, token, desktop_id, generation),
    )
    run_named_check(
        "message-rate close",
        lambda: assert_message_rate_close(api_base, token, desktop_id, generation),
    )


def run_process_terminate(
    api_base: str,
    token: bytes,
    desktop_id: str,
    generation: str,
    process_file: str,
    result_file: str,
) -> None:
    process = read_process_ref(process_file, generation)
    expected_view = {
        "process": process,
        "state": "exited",
        "exit": {"code": None, "signal": 15, "core_dumped": False},
    }
    client = WebSocket(api_base, token)
    terminal_result: dict[str, Any] | None = None
    try:
        client.connect()
        welcome = hello(client, desktop_id, generation)
        require(welcome["resume"] == {"status": "not_requested"}, "process-event session unexpectedly resumed")

        subscribe_id = new_id()
        client.send_json(
            {
                "type": "events.subscribe",
                "request_id": subscribe_id,
                "desktop_id": desktop_id,
                "desktop_generation": generation,
                "topics": ["process.exited"],
                "since_sequence": None,
            }
        )
        subscribed = expect_message(client, "events.subscribed", subscribe_id)
        require(subscribed.get("topics") == ["process.exited"], "process-event topic subscription changed")
        replay = expect_message(client, "events.replay_complete", subscribe_id)
        through_sequence = replay.get("through_sequence")
        require(isinstance(through_sequence, int) and through_sequence >= 0, "process-event replay boundary was invalid")

        command_id = new_id()
        request_id = new_id()

        def termination_message(current_request_id: str) -> dict[str, Any]:
            return {
                "type": "command.submit",
                "request_id": current_request_id,
                "command": {
                    "protocol_version": {"major": 1, "minor": 0},
                    "request_id": current_request_id,
                    "command_id": command_id,
                    "desktop_id": desktop_id,
                    "desktop_generation": generation,
                    "lease_id": None,
                    "deadline": None,
                    "trace_policy": "detailed",
                    "command": {"type": "process_terminate", "process": process, "grace_ms": 500},
                },
            }

        client.send_json(termination_message(request_id))
        event_seen = False
        last_sequence = through_sequence
        deadline = time.monotonic() + 15.0
        while terminal_result is None or not event_seen:
            remaining = deadline - time.monotonic()
            require(remaining > 0, "timed out waiting for process termination and exit event")
            message = client.recv(remaining)
            require(isinstance(message, dict), "process-event session closed before completion")
            message_type = message.get("type")
            if message_type == "event":
                require(message.get("request_id") == subscribe_id, "process event used the wrong subscription ID")
                event = message.get("event")
                require(isinstance(event, dict), "process event envelope was absent")
                require(
                    event.get("desktop_id") == desktop_id and event.get("desktop_generation") == generation,
                    "process event used the wrong desktop identity",
                )
                sequence = event.get("sequence")
                require(isinstance(sequence, int) and sequence > last_sequence, "process event sequence was not monotonic")
                last_sequence = sequence
                require(event.get("topic") == "process.exited", "process-event subscription delivered another topic")
                payload = event.get("payload")
                require(isinstance(payload, dict), "process exit payload was absent")
                require(payload.get("application") == "xmessage", "process exit used the wrong application profile")
                require(payload.get("process") == expected_view, "process exit event did not match the reaped process")
                require(payload.get("termination_requested") is True, "process exit omitted requested termination")
                require(payload.get("forced_escalation") is False, "normal TERM unexpectedly escalated to SIGKILL")
                require(not event_seen, "matching process exit event was delivered more than once")
                event_seen = True
            elif message_type in {"command.accepted", "command.progress", "command.result"}:
                require(message.get("request_id") == request_id, "termination response used the wrong request ID")
                result = message.get("result")
                require(isinstance(result, dict) and result.get("command_id") == command_id, "termination response used the wrong command ID")
                if message_type == "command.result":
                    require(result.get("lifecycle") == "succeeded", "managed process termination did not succeed")
                    require(result.get("effect_stage") == "process_exited", "termination reported the wrong effect stage")
                    require(
                        result.get("outcome") == {"type": "process_terminated", "process": expected_view},
                        "termination result did not match the reaped process",
                    )
                    terminal_result = result
            else:
                raise AcceptanceFailure(f"unexpected message {message_type} during process termination")

        retry_request_id = new_id()
        client.send_json(termination_message(retry_request_id))
        while True:
            message = client.recv(8.0)
            require(isinstance(message, dict), "process-event session closed during exact retry")
            if message.get("type") == "event":
                raise AcceptanceFailure("exact termination retry emitted a duplicate process exit event")
            require(message.get("request_id") == retry_request_id, "exact retry used the wrong request ID")
            require(message.get("type") in {"command.accepted", "command.progress", "command.result"}, "exact retry returned an unexpected message")
            result = message.get("result")
            require(isinstance(result, dict) and result.get("command_id") == command_id, "exact retry changed the command ID")
            if message.get("type") == "command.result":
                require(result == terminal_result, "exact termination retry changed the immutable result")
                break

        unsubscribe_id = new_id()
        client.send_json(
            {
                "type": "events.unsubscribe",
                "request_id": unsubscribe_id,
                "desktop_id": desktop_id,
                "desktop_generation": generation,
            }
        )
        while True:
            message = client.recv(8.0)
            require(isinstance(message, dict), "process-event session closed before unsubscribe")
            require(message.get("type") != "event", "duplicate process exit arrived before unsubscribe")
            require(message.get("type") == "events.unsubscribed", "unexpected message before process-event unsubscribe")
            require(message.get("request_id") == unsubscribe_id, "process-event unsubscribe used the wrong request ID")
            break

        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        descriptor = os.open(result_file, flags, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(terminal_result, output, separators=(",", ":"), ensure_ascii=False)
            output.write("\n")
    finally:
        client.close_transport()


def open_ready(api_base: str, token: bytes, desktop_id: str, generation: str) -> WebSocket:
    client = WebSocket(api_base, token)
    client.connect()
    hello(client, desktop_id, generation)
    return client


def assert_strict_hello_close(api_base: str, token: bytes) -> None:
    client = WebSocket(api_base, token)
    client.connect()
    client.send_json(
        {
            "type": "client.hello",
            "request_id": new_id(),
            "protocol": {"major": 1, "min_minor": 0, "max_minor": 0},
            "client": {"name": "xenoteer-phase3-blackbox", "version": "1.0.0"},
            "resume": None,
            "unknown": True,
        }
    )
    error = expect_message(client, "error")
    require(error.get("code") == "invalid_request" and error.get("request_id") is None, "strict hello rejection returned the wrong error")
    close = client.recv()
    require(isinstance(close, Close) and close.code == 1002, "invalid strict hello did not close with 1002")
    client.close_transport()


def assert_invalid_message_close(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    client = open_ready(api_base, token, desktop_id, generation)
    client.send_frame(0x1, b"{not-json")
    error = expect_message(client, "error")
    require(error.get("code") == "invalid_request", "malformed JSON returned the wrong error")
    close = client.recv()
    require(isinstance(close, Close) and close.code == 1007, "malformed JSON did not close with 1007")
    client.close_transport()


def assert_invalid_utf8_close(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    client = open_ready(api_base, token, desktop_id, generation)
    client.send_frame(0x1, b"\xff")
    close = client.recv()
    require(isinstance(close, Close) and close.code == 1007, "invalid UTF-8 text did not close with 1007")
    client.close_transport()


def assert_binary_message_close(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    client = open_ready(api_base, token, desktop_id, generation)
    client.send_frame(0x2, b"{}")
    close = client.recv()
    require(isinstance(close, Close) and close.code == 1003, "binary application message did not close with 1003")
    client.close_transport()


def assert_oversize_close(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    client = open_ready(api_base, token, desktop_id, generation)
    client.send_frame(0x1, b" " * (EXPECTED_MESSAGE_LIMIT + 1))
    close = client.recv(timeout=12.0)
    require(isinstance(close, Close) and close.code == 1009, "oversized WebSocket message did not close with 1009")
    client.close_transport()


def assert_message_rate_close(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    client = open_ready(api_base, token, desktop_id, generation)
    sent_ids: set[str] = set()
    sent_count = 0
    for index in range(48):
        request_id = new_id()
        sent_ids.add(request_id)
        try:
            client.send_json({"type": "client.ping", "request_id": request_id, "nonce": f"burst-{index}"})
            sent_count += 1
        except (BrokenPipeError, ConnectionResetError):
            break
    require(sent_count > 30, "server closed before the documented message-rate burst was consumed")
    saw_error = False
    while True:
        message = client.recv()
        if isinstance(message, Close):
            require(saw_error, "message-rate close arrived before its reserved error")
            require(message.code == 1008, "message-rate exhaustion did not close with 1008")
            break
        if message.get("type") == "error":
            require(not saw_error, "message-rate exhaustion emitted duplicate errors")
            require(message.get("code") == "resource_exhausted", "message-rate exhaustion returned the wrong error code")
            require(message.get("request_id") in sent_ids, "message-rate error used an unknown request ID")
            saw_error = True
        else:
            require(message.get("type") == "server.pong", "rate test received an unexpected application message")
    client.close_transport()


def run_draining(api_base: str, token: bytes, desktop_id: str, generation: str, container_name: str) -> None:
    require(
        1 <= len(container_name) <= 128
        and container_name[0].isalnum()
        and all(character.isalnum() or character in "_.-" for character in container_name),
        "draining container name is invalid",
    )
    client = open_ready(api_base, token, desktop_id, generation)
    stop = subprocess.Popen(
        ["docker", "stop", "--time", "40", container_name],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        draining = expect_message(client, "server.draining", timeout=30.0)
        require(draining.get("desktop_id") == desktop_id, "draining notification used the wrong desktop")
        require(draining.get("desktop_generation") == generation, "draining notification used the wrong generation")
        require(isinstance(draining.get("reason_code"), str) and draining["reason_code"], "draining notification omitted its safe reason code")
        close = client.recv(timeout=38.0)
        require(isinstance(close, Close) and close.code == 1001, "server did not close 1001 after draining notification")
        status = stop.wait(timeout=45.0)
        require(status == 0, "Docker stop failed after WebSocket draining notification")
    finally:
        client.close_transport()
        if stop.poll() is None:
            stop.kill()
            stop.wait()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("exercise", "process-terminate", "draining"))
    parser.add_argument("--api-base", required=True)
    parser.add_argument("--token-file", required=True)
    parser.add_argument("--desktop-id", required=True)
    parser.add_argument("--desktop-generation", required=True)
    parser.add_argument("--container-name")
    parser.add_argument("--process-file")
    parser.add_argument("--result-file")
    args = parser.parse_args()
    if args.mode == "draining" and not args.container_name:
        parser.error("draining mode requires --container-name")
    if args.mode == "process-terminate" and (not args.process_file or not args.result_file):
        parser.error("process-terminate mode requires --process-file and --result-file")
    if args.mode != "draining" and args.container_name:
        parser.error(f"{args.mode} mode does not accept --container-name")
    if args.mode != "process-terminate" and (args.process_file or args.result_file):
        parser.error(f"{args.mode} mode does not accept process file arguments")
    return args


def main() -> int:
    args = parse_args()
    token = read_token(args.token_file)
    uuid.UUID(args.desktop_id)
    uuid.UUID(args.desktop_generation)
    if args.mode == "exercise":
        run_exercise(args.api_base, token, args.desktop_id, args.desktop_generation)
        print("Phase 3 live WebSocket acceptance passed")
    elif args.mode == "process-terminate":
        run_process_terminate(
            args.api_base,
            token,
            args.desktop_id,
            args.desktop_generation,
            args.process_file,
            args.result_file,
        )
        print("Phase 3 live process-exit event acceptance passed")
    else:
        run_draining(args.api_base, token, args.desktop_id, args.desktop_generation, args.container_name)
        print("Phase 3 WebSocket draining-order acceptance passed")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (AcceptanceFailure, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Phase 3 WebSocket acceptance failed: {error}", file=sys.stderr)
        sys.exit(1)
