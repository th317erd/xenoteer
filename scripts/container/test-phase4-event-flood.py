#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Black-box slow-subscriber acceptance under real X11 observation churn."""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import select
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from types import ModuleType
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
PHASE3_CLIENT = REPO_ROOT / "scripts/container/test-phase3-websocket.py"
CHURN_BINARY = "/run/xenoteer/phase4-event-flood/x11-window-churn"
CONTAINER_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")


def load_websocket_client() -> ModuleType:
    spec = importlib.util.spec_from_file_location("xenoteer_phase3_websocket", PHASE3_CLIENT)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load the reviewed WebSocket acceptance client")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


ws = load_websocket_client()
AcceptanceFailure = ws.AcceptanceFailure
require = ws.require


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--api-base", required=True)
    parser.add_argument("--token-file", required=True)
    parser.add_argument("--desktop-id", required=True)
    parser.add_argument("--desktop-generation", required=True)
    parser.add_argument("--container-name", required=True)
    args = parser.parse_args()
    if not CONTAINER_NAME.fullmatch(args.container_name):
        parser.error("container name is outside the reviewed Docker identifier subset")
    return args


def subscribe(client: Any, desktop_id: str, generation: str) -> str:
    request_id = ws.new_id()
    client.send_json(
        {
            "type": "events.subscribe",
            "request_id": request_id,
            "desktop_id": desktop_id,
            "desktop_generation": generation,
            "topics": [],
            "since_sequence": None,
        }
    )
    subscribed = ws.expect_message(client, "events.subscribed", request_id)
    require(subscribed.get("topics") == [], "server changed the all-topic subscription")
    replay = ws.expect_message(client, "events.replay_complete", request_id)
    require(
        replay.get("desktop_id") == desktop_id
        and replay.get("desktop_generation") == generation
        and parse_u64_string(replay.get("through_sequence")) is not None,
        "initial replay boundary was invalid",
    )
    return request_id


def launch_churn(container_name: str) -> tuple[subprocess.Popen[bytes], int]:
    process = subprocess.Popen(
        [
            "docker",
            "exec",
            container_name,
            "/command/s6-envdir",
            "/run/xenoteer/env",
            "/command/s6-setuidgid",
            "xenoteer",
            CHURN_BINARY,
            "--iterations",
            "16384",
            "--batch-size",
            "256",
            "--batch-pause-ms",
            "5",
            "--start-delay-ms",
            "3000",
            "--hold-after-ms",
            "8000",
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    require(process.stdout is not None, "churn fixture stdout pipe was unavailable")
    readable, _, _ = select.select([process.stdout], [], [], 4.0)
    require(bool(readable), "X11 churn fixture did not report readiness")
    line = process.stdout.readline(4097)
    require(0 < len(line) <= 4096, "X11 churn fixture readiness was absent or oversized")
    try:
        ready = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AcceptanceFailure("X11 churn fixture readiness was malformed") from error
    require(
        ready.get("type") == "ready"
        and valid_positive_integer(ready.get("sentinel_window"))
        and ready.get("iterations") == 16384
        and ready.get("batch_size") == 256,
        "X11 churn fixture readiness evidence changed",
    )
    require(process.poll() is None, "X11 churn fixture exited before the concurrency probe")
    return process, int(ready["sentinel_window"])


def wait_churn_started(process: subprocess.Popen[bytes]) -> None:
    require(process.stdout is not None, "churn fixture stdout pipe was unavailable")
    readable, _, _ = select.select([process.stdout], [], [], 5.0)
    require(bool(readable), "X11 churn fixture did not start within its bound")
    line = process.stdout.readline(4097)
    require(0 < len(line) <= 4096, "X11 churn start evidence was absent or oversized")
    try:
        started = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AcceptanceFailure("X11 churn start evidence was malformed") from error
    require(started == {"type": "churn_started"}, "X11 churn start evidence changed")
    require(process.poll() is None, "X11 churn exited at its start boundary")


def submit_probe(
    api_base: str,
    token: bytes,
    desktop_id: str,
    generation: str,
    churn: subprocess.Popen[bytes],
) -> float:
    client = ws.WebSocket(api_base, token)
    client.connect()
    ws.hello(client, desktop_id, generation)
    request_id = ws.new_id()
    command_id = ws.new_id()
    started = time.monotonic()
    client.send_json(
        {
            "type": "command.submit",
            "request_id": request_id,
            "command": {
                "protocol_version": {"major": 1, "minor": 0},
                "request_id": request_id,
                "command_id": command_id,
                "desktop_id": desktop_id,
                "desktop_generation": generation,
                "lease_id": None,
                "deadline": None,
                "trace_policy": "detailed",
                "command": {"type": "desktop_probe"},
            },
        }
    )
    deadline = started + 5.0
    try:
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, "independent command was not responsive during X11 churn")
            message = client.recv(remaining)
            require(isinstance(message, dict), "independent command connection closed")
            require(
                message.get("type") in {"command.accepted", "command.progress", "command.result"},
                "independent command received an unexpected message",
            )
            require(message.get("request_id") == request_id, "independent command request ID changed")
            result = message.get("result")
            require(
                isinstance(result, dict) and result.get("command_id") == command_id,
                "independent command result identity changed",
            )
            if message.get("type") != "command.result":
                continue
            require(
                result.get("lifecycle") == "succeeded"
                and result.get("effect_stage") == "none"
                and result.get("outcome") == {"type": "probe", "ready": True},
                "independent desktop probe did not succeed exactly",
            )
            elapsed = time.monotonic() - started
            require(elapsed <= 5.0, "independent command exceeded its responsiveness bound")
            require(churn.poll() is None, "X11 churn ended before the independent command completed")
            return elapsed
    finally:
        client.close_transport()


def finish_churn(process: subprocess.Popen[bytes]) -> None:
    try:
        stdout, stderr = process.communicate(timeout=20.0)
    except subprocess.TimeoutExpired as error:
        process.kill()
        process.communicate()
        raise AcceptanceFailure("X11 churn fixture exceeded its completion bound") from error
    require(process.returncode == 0, "X11 churn fixture failed")
    require(len(stdout) <= 4096 and len(stderr) <= 4096, "X11 churn fixture output was oversized")


def expect_terminal_resync(
    client: Any,
    subscription_id: str,
    desktop_id: str,
    generation: str,
) -> tuple[str, int]:
    deadline = time.monotonic() + 15.0
    last_sequence = 0
    event_count = 0
    while True:
        remaining = deadline - time.monotonic()
        require(remaining > 0, "slow subscriber did not receive a fail-closed resync")
        message = client.recv(remaining)
        require(isinstance(message, dict), "slow subscriber closed before fail-closed resync")
        message_type = message.get("type")
        require(
            message.get("request_id") == subscription_id,
            "slow subscriber received an unrelated application message",
        )
        if message_type == "event":
            event = message.get("event")
            require(isinstance(event, dict), "slow subscriber event envelope was absent")
            sequence = parse_u64_string(event.get("sequence"), minimum=1)
            require(
                event.get("desktop_id") == desktop_id
                and event.get("desktop_generation") == generation
                and sequence is not None
                and sequence > last_sequence,
                "slow subscriber event continuity was invalid before resync",
            )
            last_sequence = sequence
            event_count += 1
            require(event_count <= 100_000, "slow subscriber exceeded its event drain bound")
            continue
        require(message_type == "events.resync_required", "slow subscriber received an unexpected message")
        reason = message.get("reason")
        dropped_through = parse_u64_string(message.get("dropped_through"), minimum=1)
        latest_sequence = parse_u64_string(message.get("latest_sequence"), minimum=1)
        require(
            reason == "history_lost",
            "slow subscriber did not receive the raw observation-gap history barrier",
        )
        require(
            message.get("desktop_id") == desktop_id
            and message.get("desktop_generation") == generation
            and dropped_through is not None
            and latest_sequence is not None
            and dropped_through <= latest_sequence,
            "slow subscriber resync evidence was invalid",
        )
        return str(reason), event_count


def prove_subscription_ended(
    client: Any,
    subscription_id: str,
    desktop_id: str,
    generation: str,
) -> None:
    ping_id = ws.new_id()
    nonce = f"phase4-event-flood-{ws.new_id()}"
    client.send_json({"type": "client.ping", "request_id": ping_id, "nonce": nonce})
    deadline = time.monotonic() + 8.0
    while True:
        remaining = deadline - time.monotonic()
        require(remaining > 0, "post-resync WebSocket ping timed out")
        message = client.recv(remaining)
        require(isinstance(message, dict), "WebSocket closed after subscription resync")
        require(
            not (message.get("type") == "event" and message.get("request_id") == subscription_id),
            "terminated subscription delivered an event after resync",
        )
        if message.get("type") == "server.pong":
            require(
                message.get("request_id") == ping_id and message.get("nonce") == nonce,
                "post-resync WebSocket pong changed correlation",
            )
            break
        raise AcceptanceFailure("post-resync WebSocket emitted an unexpected message")

    replacement = subscribe(client, desktop_id, generation)
    unsubscribe_id = ws.new_id()
    client.send_json(
        {
            "type": "events.unsubscribe",
            "request_id": unsubscribe_id,
            "desktop_id": desktop_id,
            "desktop_generation": generation,
        }
    )
    deadline = time.monotonic() + 8.0
    interleaved_events = 0
    while True:
        remaining = deadline - time.monotonic()
        require(remaining > 0, "replacement unsubscribe acknowledgement timed out")
        message = client.recv(remaining)
        require(isinstance(message, dict), "WebSocket closed before replacement unsubscribe")
        if (
            message.get("type") == "events.unsubscribed"
            and message.get("request_id") == unsubscribe_id
        ):
            unsubscribed = message
            break
        require(
            message.get("type") == "event"
            and message.get("request_id") == replacement
            and isinstance(message.get("event"), dict),
            "replacement unsubscribe received an unrelated application message",
        )
        interleaved_events += 1
        require(
            interleaved_events <= 4_096,
            "replacement unsubscribe exceeded its interleaved-event drain bound",
        )
    require(replacement != subscription_id, "replacement subscription reused request identity")
    require(unsubscribed.get("request_id") == unsubscribe_id, "replacement unsubscribe changed identity")


def authoritative_json(
    api_base: str,
    token: bytes,
    path: str,
    *,
    timeout: float = 5.0,
) -> dict[str, Any]:
    request = urllib.request.Request(
        f"{api_base.rstrip('/')}{path}",
        headers={
            "Authorization": f"Bearer {token.decode('ascii')}",
            "Accept": "application/json, application/problem+json",
        },
        method="GET",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            require(response.status == 200, "authoritative window request returned a non-200 status")
            require(len(response.headers.as_bytes()) <= 64 * 1024, "window response headers were oversized")
            body = response.read(2 * 1024 * 1024 + 1)
    except urllib.error.HTTPError as error:
        raise AcceptanceFailure(f"authoritative window request returned HTTP {error.code}") from error
    require(len(body) <= 2 * 1024 * 1024, "authoritative window response was oversized")
    try:
        value = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AcceptanceFailure("authoritative window response was not valid JSON") from error
    require(isinstance(value, dict), "authoritative window response was not an object")
    return value


def list_window_page(api_base: str, token: bytes, desktop_id: str, generation: str) -> dict[str, Any]:
    query = urllib.parse.urlencode(
        {
            "desktop_generation": generation,
            "limit": 200,
            "order": "creation_ascending",
        }
    )
    return authoritative_json(
        api_base,
        token,
        f"/v1/desktops/{desktop_id}/windows?{query}",
    )


def capture_sentinel_authority(
    api_base: str,
    token: bytes,
    desktop_id: str,
    generation: str,
    sentinel_xid: int,
) -> dict[str, Any]:
    deadline = time.monotonic() + 2.5
    while time.monotonic() < deadline:
        page = list_window_page(api_base, token, desktop_id, generation)
        for entry in page.get("windows", []):
            snapshot = entry.get("snapshot", {})
            reference = snapshot.get("ref")
            if isinstance(reference, dict) and reference.get("xid") == sentinel_xid:
                token_value = entry.get("reference_token")
                require(
                    reference.get("desktop_id") == desktop_id
                    and reference.get("desktop_generation") == generation
                    and parse_u64_string(
                        reference.get("observed_generation"), minimum=1
                    )
                    is not None
                    and isinstance(reference.get("identity_hash"), str)
                    and isinstance(token_value, str),
                    "pre-gap sentinel authority was malformed",
                )
                return {"reference": reference, "token": token_value}
        time.sleep(0.05)
    raise AcceptanceFailure("sentinel authority was not captured before X11 churn")


def require_stale_token(
    api_base: str,
    token: bytes,
    desktop_id: str,
    generation: str,
    token_value: str,
) -> None:
    query = urllib.parse.urlencode({"desktop_generation": generation})
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        request = urllib.request.Request(
            f"{api_base.rstrip('/')}/v1/desktops/{desktop_id}/windows/"
            f"{urllib.parse.quote(token_value, safe='')}?{query}",
            headers={
                "Authorization": f"Bearer {token.decode('ascii')}",
                "Accept": "application/json, application/problem+json",
            },
            method="GET",
        )
        try:
            with urllib.request.urlopen(request, timeout=5.0) as response:
                body = response.read(2 * 1024 * 1024 + 1)
                require(len(body) <= 2 * 1024 * 1024, "pre-gap token response was oversized")
        except urllib.error.HTTPError as error:
            body = error.read(2 * 1024 * 1024 + 1)
            require(error.code == 404, "pre-gap sentinel token failed with the wrong status")
            require(len(body) <= 2 * 1024 * 1024, "stale-token problem response was oversized")
            return
        time.sleep(0.05)
    raise AcceptanceFailure("pre-gap sentinel token remained valid after observation loss")


def require_reminted_sentinel(
    api_base: str,
    token: bytes,
    desktop_id: str,
    generation: str,
    sentinel_xid: int,
    initial: dict[str, Any],
) -> None:
    initial_reference = initial["reference"]
    require_stale_token(
        api_base,
        token,
        desktop_id,
        generation,
        initial["token"],
    )
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        page = list_window_page(api_base, token, desktop_id, generation)
        for entry in page.get("windows", []):
            reference = entry.get("snapshot", {}).get("ref")
            if not isinstance(reference, dict) or reference.get("xid") != sentinel_xid:
                continue
            require(
                reference.get("desktop_id") == desktop_id
                and reference.get("desktop_generation") == generation
                and (
                    observed_generation := parse_u64_string(
                        reference.get("observed_generation"), minimum=1
                    )
                )
                is not None
                and observed_generation
                > int(initial_reference["observed_generation"])
                and reference.get("identity_hash") != initial_reference.get("identity_hash")
                and reference != initial_reference,
                "post-gap sentinel XID was not reminted with a fresh exact identity",
            )
            return
        time.sleep(0.05)
    raise AcceptanceFailure("still-live sentinel was absent after observation model rebuild")


def prove_coherent_snapshot(api_base: str, token: bytes, desktop_id: str, generation: str) -> None:
    deadline = time.monotonic() + 12.0
    page: dict[str, Any] | None = None
    while time.monotonic() < deadline:
        try:
            candidate = list_window_page(api_base, token, desktop_id, generation)
        except AcceptanceFailure as error:
            if "HTTP 503" not in str(error) and "HTTP 504" not in str(error):
                raise
            time.sleep(0.1)
            continue
        windows = candidate.get("windows")
        if isinstance(windows, list) and windows:
            page = candidate
            break
        time.sleep(0.1)
    require(page is not None, "authoritative window model did not recover after resync")
    windows = page["windows"]
    revision = page.get("snapshot_revision")
    require(
        page.get("desktop_id") == desktop_id
        and page.get("desktop_generation") == generation
        and parse_u64_string(revision, minimum=1) is not None
        and page.get("next_cursor") is None
        and len(windows) <= 200,
        "authoritative window page scope or bounds were incoherent",
    )
    identities: set[tuple[int, int]] = set()
    for entry in windows:
        require(isinstance(entry, dict), "window page entry was not an object")
        snapshot = entry.get("snapshot")
        require(isinstance(snapshot, dict), "window page snapshot was absent")
        reference = snapshot.get("ref")
        require(isinstance(reference, dict), "window page reference was absent")
        identity = (reference.get("xid"), reference.get("observed_generation"))
        require(
            reference.get("desktop_id") == desktop_id
            and reference.get("desktop_generation") == generation
            and valid_positive_integer(identity[0])
            and parse_u64_string(identity[1], minimum=1) is not None
            and snapshot.get("model_revision") == revision
            and identity not in identities,
            "window page contained stale, duplicate, or cross-revision evidence",
        )
        identities.add(identity)

    entry = windows[0]
    token_value = entry.get("reference_token")
    require(isinstance(token_value, str) and 1 <= len(token_value) <= 4096, "window token was invalid")
    snapshot_query = urllib.parse.urlencode({"desktop_generation": generation})
    exact = authoritative_json(
        api_base,
        token,
        f"/v1/desktops/{desktop_id}/windows/{urllib.parse.quote(token_value, safe='')}?{snapshot_query}",
    )
    exact_entry = exact.get("window")
    require(isinstance(exact_entry, dict), "exact window snapshot entry was absent")
    exact_snapshot = exact_entry.get("snapshot")
    require(isinstance(exact_snapshot, dict), "exact window snapshot was absent")
    require(
        exact.get("snapshot_revision") == exact_snapshot.get("model_revision")
        and exact_snapshot.get("ref") == entry["snapshot"].get("ref"),
        "token lookup did not preserve exact generation-bound window identity",
    )


def valid_positive_integer(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def valid_nonnegative_integer(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def parse_u64_string(value: Any, *, minimum: int = 0) -> int | None:
    if not isinstance(value, str) or re.fullmatch(r"0|[1-9][0-9]{0,19}", value) is None:
        return None
    parsed = int(value)
    if parsed < minimum or parsed > (1 << 64) - 1:
        return None
    return parsed


def run(args: argparse.Namespace) -> tuple[str, int, float]:
    token = ws.read_token(args.token_file)
    uuid.UUID(args.desktop_id)
    uuid.UUID(args.desktop_generation)
    slow = ws.WebSocket(args.api_base, token)
    slow.connect()
    welcome = ws.hello(slow, args.desktop_id, args.desktop_generation)
    subscription_id = subscribe(slow, args.desktop_id, args.desktop_generation)
    slow.set_receive_buffer(4096)
    churn, sentinel_xid = launch_churn(args.container_name)
    try:
        initial_sentinel = capture_sentinel_authority(
            args.api_base,
            token,
            args.desktop_id,
            args.desktop_generation,
            sentinel_xid,
        )
        wait_churn_started(churn)
        probe_seconds = submit_probe(
            args.api_base,
            token,
            args.desktop_id,
            args.desktop_generation,
            churn,
        )
        reason, event_count = expect_terminal_resync(
            slow,
            subscription_id,
            args.desktop_id,
            args.desktop_generation,
        )
        require_reminted_sentinel(
            args.api_base,
            token,
            args.desktop_id,
            args.desktop_generation,
            sentinel_xid,
            initial_sentinel,
        )
        finish_churn(churn)
        prove_subscription_ended(
            slow,
            subscription_id,
            args.desktop_id,
            args.desktop_generation,
        )
        prove_coherent_snapshot(
            args.api_base,
            token,
            args.desktop_id,
            args.desktop_generation,
        )
        require(
            welcome["limits"]["normal_outbound_capacity"] > 0
            and welcome["limits"]["reserved_outbound_capacity"] > 0,
            "slow-subscriber test did not exercise bounded outbound queues",
        )
        return reason, event_count, probe_seconds
    finally:
        slow.close_transport()
        if churn.poll() is None:
            churn.kill()
            churn.communicate()


def main() -> int:
    args = parse_args()
    reason, event_count, probe_seconds = run(args)
    print(
        "Phase 4 live event-flood acceptance passed: "
        f"resync={reason}, pre_resync_events={event_count}, "
        f"concurrent_probe_ms={round(probe_seconds * 1000)}"
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (AcceptanceFailure, OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"Phase 4 event-flood acceptance failed: {error}", file=sys.stderr)
        sys.exit(1)
