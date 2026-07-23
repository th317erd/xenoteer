#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Live Phase-4 API acceptance against the desktop-application fixture image."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURE_ROOT = "/usr/share/xenoteer/fixtures/desktop-apps"
CLIPBOARD_HELPER = f"{FIXTURE_ROOT}/phase4-clipboard.py"
ATSPI_HELPER = f"{FIXTURE_ROOT}/phase4-atspi-text.py"
TOKEN_CANARY = b"PHASE4_LIVE_FIXTURE_TOKEN_MUST_NOT_APPEAR_IN_LOGS_0123456789AB"
INITIAL_ENTRY = b"editable fixture text"
INCR_BYTES = 384 * 1024
MAX_SCREENSHOT_BYTES = 32 * 1024 * 1024
CONTENT_CANARIES = (
    b"phase4-direct-owner",
    b"phase4-incr-owner:",
    b"phase4-direct-set",
    b"phase4-restore-canary",
    b"phase4-incr-source:",
    b"-phase4-gtk3",
    b"-phase4-qt6",
    b"-phase4-chromium",
    b"-phase4-firefox",
    b"-phase4-qtwebengine",
)


class AcceptanceError(RuntimeError):
    """One content-safe acceptance failure."""


@dataclass(frozen=True)
class ApplicationFixture:
    name: str
    title: str
    entry_name: str
    initial_text: bytes
    command: tuple[str, ...]
    stop_pattern: str


APPLICATIONS = (
    ApplicationFixture(
        "gtk3",
        "Xenoteer GTK3 Fixture — Main",
        # GtkEntryBuffer truncates very large inserts at 65,534 characters;
        # exercise the 384-KiB INCR path against the unbounded GtkTextView.
        "Stable Text Area",
        b"editable multiline fixture text",
        (f"{FIXTURE_ROOT}/gtk3-fixture.py",),
        "gtk3-fixture.py",
    ),
    ApplicationFixture(
        "qt6",
        "Xenoteer Qt6 Fixture — Main",
        "Stable Entry",
        INITIAL_ENTRY,
        (f"{FIXTURE_ROOT}/qt6-fixture.py",),
        "qt6-fixture.py",
    ),
    ApplicationFixture(
        "chromium",
        "Xenoteer Chromium Browser Fixture",
        "Stable entry",
        INITIAL_ENTRY,
        (f"{FIXTURE_ROOT}/launch-chromium-fixture",),
        "chromium",
    ),
    ApplicationFixture(
        "firefox",
        "Xenoteer Firefox Browser Fixture",
        "Stable entry",
        INITIAL_ENTRY,
        (f"{FIXTURE_ROOT}/launch-firefox-fixture",),
        "firefox",
    ),
    ApplicationFixture(
        "qtwebengine",
        "Xenoteer QtWebEngine Browser Fixture",
        "Stable entry",
        INITIAL_ENTRY,
        (f"{FIXTURE_ROOT}/launch-qtwebengine-fixture",),
        "qtwebengine-fixture.py|QtWebEngineProcess",
    ),
)


def run(
    *arguments: str,
    check: bool = True,
    capture_output: bool = True,
    text: bool = True,
    timeout: float | None = 60,
) -> subprocess.CompletedProcess:
    result = subprocess.run(
        arguments,
        check=False,
        capture_output=capture_output,
        text=text,
        timeout=timeout,
    )
    if check and result.returncode != 0:
        stderr = result.stderr.strip() if isinstance(result.stderr, str) else ""
        raise AcceptanceError(
            f"command failed safely ({result.returncode}): {arguments[0]}"
            + (f": {stderr[:500]}" if stderr else "")
        )
    return result


def docker(*arguments: str, **kwargs: Any) -> subprocess.CompletedProcess:
    return run("docker", *arguments, **kwargs)


class LiveContainer:
    def __init__(
        self,
        image: str,
        token_file: Path,
        daemon_override: Path | None = None,
    ) -> None:
        self.image = image
        self.token_file = token_file
        self.daemon_override = daemon_override
        self.name = f"xenoteer-phase4-live-{os.getpid()}"
        self.created = False

    def start(self) -> None:
        seccomp = REPO_ROOT / "container/spikes/browser/seccomp_profile.json"
        # The diagnostic override exists specifically for the pre-UID-split
        # cached fixture image, whose daemon still runs as desktop UID 1000.
        # Coherent current images use the dedicated daemon UID/GID 1001.
        artifact_owner = 1000 if self.daemon_override is not None else 1001
        fixture_source = (
            REPO_ROOT
            / "container/rootfs/usr/share/xenoteer/fixtures/desktop-apps"
        )
        keyboard_profile = (
            REPO_ROOT
            / "container/rootfs/usr/share/xenoteer/profiles/common/xdg/config/xfce4/xfconf/xfce-perchannel-xml/keyboard-layout.xml"
        )
        xfce_launcher = (
            REPO_ROOT / "container/rootfs/usr/local/libexec/xenoteer/run-xfce"
        )
        arguments = [
            "run",
            "--detach",
            "--name",
            self.name,
            "--cpus",
            "2",
            "--memory",
            "6g",
            "--pids-limit",
            "512",
            "--shm-size",
            "4g",
            "--tmpfs",
            "/run/xenoteer/artifacts:rw,noexec,nosuid,nodev,size=512m,mode=0700,"
            f"uid={artifact_owner},gid={artifact_owner}",
            "--log-driver",
            "json-file",
            "--log-opt",
            "max-size=2m",
            "--log-opt",
            "max-file=1",
            "--security-opt",
            f"seccomp={seccomp}",
            "--publish",
            "127.0.0.1::8080",
            "--env",
            "DESKTOP_PROFILE=bare",
            "--volume",
            f"{self.token_file}:/run/secrets/xenoteer_api_token:ro",
        ]
        if self.daemon_override is not None:
            arguments.extend(
                (
                    # Diagnostic mode deliberately overlays all coupled files
                    # needed to run the current daemon against an older image.
                    # The release/CI gate omits the override and tests only the
                    # immutable derived image contents.
                    "--volume",
                    f"{fixture_source / 'phase4-atspi-text.py'}:{ATSPI_HELPER}:ro",
                    "--volume",
                    f"{fixture_source / 'phase4-clipboard.py'}:{CLIPBOARD_HELPER}:ro",
                    "--volume",
                    f"{keyboard_profile}:/usr/share/xenoteer/profiles/common/xdg/config/xfce4/xfconf/xfce-perchannel-xml/keyboard-layout.xml:ro",
                    "--volume",
                    f"{xfce_launcher}:/usr/local/libexec/xenoteer/run-xfce:ro",
                    "--volume",
                    f"{self.daemon_override}:/usr/local/bin/xenoteerd:ro",
                )
            )
        arguments.append(self.image)
        docker(*arguments)
        self.created = True

    def remove(self) -> None:
        if self.created:
            docker(
                "rm",
                "--force",
                "--volumes",
                self.name,
                check=False,
                timeout=45,
            )
            self.created = False

    def safe_logs(self) -> str:
        if not self.created:
            return ""
        result = docker("logs", self.name, check=False, timeout=20)
        logs = (result.stdout + result.stderr).encode("utf-8", errors="replace")
        if TOKEN_CANARY in logs or any(canary in logs for canary in CONTENT_CANARIES):
            return "container logs suppressed because they contain an acceptance canary"
        return logs.decode("utf-8", errors="replace")[-16_000:]

    def exec(
        self,
        *arguments: str,
        user: int | None = None,
        desktop: bool = False,
        detach: bool = False,
        check: bool = True,
        timeout: float | None = 60,
    ) -> subprocess.CompletedProcess:
        command = ["exec"]
        if detach:
            command.append("--detach")
        if user is not None:
            command.extend(("--user", str(user)))
        command.append(self.name)
        if desktop:
            command.extend(("/command/s6-envdir", "-f", "-L", "/run/xenoteer/env"))
        command.extend(arguments)
        return docker(*command, check=check, timeout=timeout)

    def wait_ready(self) -> None:
        for _ in range(240):
            manager = self.exec(
                "wmctrl", "-m", user=1000, desktop=True, check=False, timeout=4
            )
            daemon = self.exec("pgrep", "-x", "xenoteerd", check=False, timeout=4)
            if manager.returncode == 0 and daemon.returncode == 0:
                return
            running = docker(
                "inspect", self.name, "--format", "{{.State.Running}}", timeout=4
            ).stdout.strip()
            if running != "true":
                raise AcceptanceError("fixture container stopped before desktop readiness")
            time.sleep(0.25)
        raise AcceptanceError("fixture desktop did not become ready")

    def api_port(self) -> int:
        output = docker("port", self.name, "8080/tcp").stdout.strip()
        for line in output.splitlines():
            prefix = "127.0.0.1:"
            if line.startswith(prefix) and line[len(prefix) :].isdigit():
                return int(line[len(prefix) :])
        raise AcceptanceError("Docker returned no loopback API port")

    def require_fixtures(self) -> None:
        paths = [ATSPI_HELPER, CLIPBOARD_HELPER]
        paths.extend(fixture.command[0] for fixture in APPLICATIONS)
        for path in paths:
            result = self.exec("test", "-x", path, check=False)
            if result.returncode != 0:
                raise AcceptanceError(f"expected fixture is absent or not executable: {path}")
        keyboard_layout_disabled = self.exec(
            "xfconf-query",
            "-c",
            "keyboard-layout",
            "-p",
            "/Default/XkbDisable",
            user=1000,
            desktop=True,
            check=False,
        )
        if (
            keyboard_layout_disabled.returncode != 0
            or keyboard_layout_disabled.stdout.strip() != "true"
        ):
            raise AcceptanceError("XFCE keyboard-layout ownership is not disabled")

    def launch(self, fixture: ApplicationFixture) -> None:
        self.exec(*fixture.command, user=1000, desktop=True, detach=True)
        for _ in range(160):
            windows = self.exec(
                "wmctrl", "-l", user=1000, desktop=True, check=False, timeout=5
            )
            if fixture.title in windows.stdout:
                return
            time.sleep(0.25)
        raise AcceptanceError(f"expected {fixture.name} window did not appear")

    def stop_fixture(self, fixture: ApplicationFixture) -> None:
        self.exec(
            "pkill",
            "-TERM",
            "-u",
            "1000",
            "-f",
            fixture.stop_pattern,
            check=False,
        )
        for _ in range(80):
            remaining = self.exec(
                "pgrep",
                "-u",
                "1000",
                "-f",
                fixture.stop_pattern,
                check=False,
                timeout=4,
            )
            if remaining.returncode != 0:
                return
            time.sleep(0.1)
        self.exec(
            "pkill",
            "-KILL",
            "-u",
            "1000",
            "-f",
            fixture.stop_pattern,
            check=False,
        )
        raise AcceptanceError(f"{fixture.name} fixture required SIGKILL")

    def copy_fixture_file(self, source: Path, destination: str) -> None:
        docker("cp", str(source), f"{self.name}:{destination}")
        self.exec("chown", "1000:1000", destination)
        self.exec("chmod", "0600", destination)

    def start_clipboard_owner(self, input_path: str, ready_path: str) -> None:
        self.stop_clipboard_owner()
        self.exec("rm", "-f", ready_path, user=1000, desktop=True)
        self.exec(
            CLIPBOARD_HELPER,
            "own",
            "--input",
            input_path,
            "--ready-file",
            ready_path,
            user=1000,
            desktop=True,
            detach=True,
        )
        for _ in range(120):
            ready = self.exec("test", "-s", ready_path, user=1000, check=False)
            if ready.returncode == 0:
                return
            time.sleep(0.1)
        raise AcceptanceError("clipboard owner did not publish readiness")

    def stop_clipboard_owner(self) -> None:
        self.exec(
            "pkill",
            "-TERM",
            "-u",
            "1000",
            "-f",
            "phase4-clipboard.py own",
            check=False,
        )
        for _ in range(40):
            remaining = self.exec(
                "pgrep",
                "-u",
                "1000",
                "-f",
                "phase4-clipboard.py own",
                check=False,
                timeout=4,
            )
            if remaining.returncode != 0:
                return
            time.sleep(0.05)
        self.exec(
            "pkill",
            "-KILL",
            "-u",
            "1000",
            "-f",
            "phase4-clipboard.py own",
            check=False,
        )
        raise AcceptanceError("clipboard owner failed to stop cleanly")

    def verify_clipboard(self, expected_path: str) -> None:
        self.exec(
            CLIPBOARD_HELPER,
            "read",
            "--expected",
            expected_path,
            user=1000,
            desktop=True,
            timeout=20,
        )

    def focus_entry(self, name: str) -> None:
        self.exec(
            ATSPI_HELPER,
            "focus",
            "--name",
            name,
            user=1000,
            desktop=True,
            timeout=40,
        )

    def verify_entry(self, name: str, expected_path: str) -> None:
        self.exec(
            ATSPI_HELPER,
            "assert-text",
            "--name",
            name,
            "--expected-file",
            expected_path,
            user=1000,
            desktop=True,
            timeout=40,
        )

    def artifact_inventory(self) -> tuple[int, int]:
        """Return only durable/temporary object counts, never artifact names."""
        result = self.exec(
            "/usr/bin/python3",
            "-c",
            (
                "import os; "
                "entries=list(os.scandir('/run/xenoteer/artifacts')); "
                "print(sum(entry.is_dir(follow_symlinks=False) and "
                "not entry.name.startswith('.') for entry in entries), "
                "sum(entry.is_dir(follow_symlinks=False) and "
                "entry.name.startswith('.tmp-') for entry in entries))"
            ),
            timeout=10,
        )
        fields = result.stdout.strip().split()
        if len(fields) != 2 or any(not field.isdecimal() for field in fields):
            raise AcceptanceError("artifact inventory did not return two bounded counts")
        return int(fields[0]), int(fields[1])


class ApiClient:
    def __init__(self, base_url: str, token: bytes) -> None:
        self.base_url = base_url.rstrip("/")
        self.authorization = "Bearer " + token.decode("ascii")
        self.desktop_id = ""
        self.generation = ""
        self.lease_id = ""

    def request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        content_type: str | None = None,
        headers: dict[str, str] | None = None,
        expected: tuple[int, ...] = (200,),
        timeout: float = 30,
    ) -> tuple[int, Any, bytes]:
        request_headers = {"Authorization": self.authorization}
        if content_type is not None:
            request_headers["Content-Type"] = content_type
        if headers:
            request_headers.update(headers)
        request = urllib.request.Request(
            self.base_url + path,
            data=body,
            method=method,
            headers=request_headers,
        )
        try:
            response = urllib.request.urlopen(request, timeout=timeout)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            payload = response.read(34 * 1024 * 1024)
            status = response.status
            response_headers = response.headers
        if status not in expected:
            problem = ""
            if response_headers.get_content_type() in {
                "application/json",
                "application/problem+json",
            }:
                try:
                    decoded = json.loads(payload)
                    detail = str(decoded.get("detail", ""))
                    encoded_detail = detail.encode("utf-8", errors="replace")
                    if TOKEN_CANARY in encoded_detail or any(
                        canary in encoded_detail for canary in CONTENT_CANARIES
                    ):
                        detail = "<redacted acceptance canary>"
                    problem = (
                        f" code={decoded.get('code', 'unknown')}"
                        f" detail={detail[:300]!r}"
                    )
                except (json.JSONDecodeError, AttributeError):
                    pass
            raise AcceptanceError(
                f"{method} {path.split('?', 1)[0]} returned HTTP {status}{problem}; "
                f"expected {expected}"
            )
        return status, response_headers, payload

    def json_request(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        *,
        expected: tuple[int, ...] = (200,),
        headers: dict[str, str] | None = None,
        timeout: float = 30,
    ) -> dict[str, Any]:
        encoded = None
        content_type = None
        if body is not None:
            encoded = json.dumps(body, separators=(",", ":")).encode("utf-8")
            content_type = "application/json"
        _, response_headers, payload = self.request(
            method,
            path,
            body=encoded,
            content_type=content_type,
            headers=headers,
            expected=expected,
            timeout=timeout,
        )
        if response_headers.get_content_type() not in {
            "application/json",
            "application/problem+json",
        }:
            raise AcceptanceError(f"{method} returned a non-JSON content type")
        try:
            value = json.loads(payload)
        except json.JSONDecodeError as error:
            raise AcceptanceError(f"{method} returned malformed JSON") from error
        if not isinstance(value, dict):
            raise AcceptanceError(f"{method} returned a non-object JSON body")
        return value

    def discover(self) -> None:
        status = self.json_request("GET", "/v1/status")
        desktop = status.get("desktop", {})
        if desktop.get("state") not in {"ready", "degraded"}:
            raise AcceptanceError("authenticated status is not ready")
        self.desktop_id = str(desktop.get("id", ""))
        self.generation = str(desktop.get("generation", ""))
        uuid.UUID(self.desktop_id)
        uuid.UUID(self.generation)

    def acquire_lease(self) -> None:
        body = {
            "protocol_version": {"major": 1, "minor": 0},
            "request_id": str(uuid.uuid4()),
            "desktop_id": self.desktop_id,
            "desktop_generation": self.generation,
            "ttl_ms": 60_000,
        }
        state = self.json_request(
            "POST",
            f"/v1/desktops/{self.desktop_id}/lease",
            body,
            expected=(201,),
        )
        lease_id = state.get("lease_id")
        if not isinstance(lease_id, str):
            raise AcceptanceError("lease response omitted caller-owned lease ID")
        uuid.UUID(lease_id)
        self.lease_id = lease_id

    def ensure_lease(self) -> None:
        state = self.json_request(
            "GET", f"/v1/desktops/{self.desktop_id}/lease"
        )
        availability = state.get("state")
        if availability == "vacant":
            self.acquire_lease()
            return
        if availability != "held_by_caller":
            raise AcceptanceError("controller lease is unexpectedly unavailable")
        lease_id = state.get("lease_id")
        if not isinstance(lease_id, str):
            raise AcceptanceError("caller-owned lease state omitted its lease ID")
        uuid.UUID(lease_id)
        self.lease_id = lease_id
        body = {
            "protocol_version": {"major": 1, "minor": 0},
            "request_id": str(uuid.uuid4()),
            "desktop_id": self.desktop_id,
            "desktop_generation": self.generation,
            "lease_id": lease_id,
            "ttl_ms": 60_000,
        }
        renewed = self.json_request(
            "POST",
            f"/v1/desktops/{self.desktop_id}/lease/{lease_id}/renew",
            body,
        )
        if renewed.get("state") != "held_by_caller":
            raise AcceptanceError("controller lease renewal did not remain caller-owned")

    def submit(
        self,
        command: dict[str, Any],
        *,
        lease: bool = False,
        timeout: float = 90,
    ) -> dict[str, Any]:
        command_id = str(uuid.uuid4())
        envelope: dict[str, Any] = {
            "protocol_version": {"major": 1, "minor": 0},
            "request_id": str(uuid.uuid4()),
            "command_id": command_id,
            "desktop_id": self.desktop_id,
            "desktop_generation": self.generation,
            "command": command,
        }
        if lease:
            envelope["lease_id"] = self.lease_id
        result = self.json_request(
            "POST",
            f"/v1/desktops/{self.desktop_id}/commands",
            envelope,
            expected=(200, 202),
            headers={"Idempotency-Key": command_id},
            timeout=timeout,
        )
        deadline = time.monotonic() + timeout
        while result.get("lifecycle") in {"accepted", "running"}:
            if time.monotonic() >= deadline:
                raise AcceptanceError("command did not become terminal before test deadline")
            result = self.json_request(
                "GET",
                f"/v1/desktops/{self.desktop_id}/commands/{command_id}/wait?timeout_ms=5000",
                expected=(200, 202),
                timeout=8,
            )
        if result.get("lifecycle") != "succeeded":
            error = result.get("error") or {}
            safe_error = {
                key: error.get(key)
                for key in ("code", "title", "retry", "effect_stage")
                if error.get(key) is not None
            }
            raise AcceptanceError(
                f"command {command.get('type')} failed safely: "
                + json.dumps(safe_error, sort_keys=True)[:800]
            )
        return result

    @staticmethod
    def selector(title: str) -> dict[str, Any]:
        return {
            "type": "predicate",
            "predicate": {
                "type": "text",
                "field": "title",
                "matcher": {
                    "type": "contains",
                    "value": title,
                    "case_sensitive": True,
                },
            },
        }

    @staticmethod
    def snapshot_title(snapshot: dict[str, Any]) -> str | None:
        title = snapshot.get("metadata", {}).get("title")
        return title.get("value") if isinstance(title, dict) else None

    def resolve_window(self, title: str) -> dict[str, Any]:
        query = urllib.parse.urlencode(
            {
                "desktop_generation": self.generation,
                "limit": 200,
                "order": "creation_ascending",
            }
        )
        for _ in range(120):
            listed = self.json_request(
                "GET", f"/v1/desktops/{self.desktop_id}/windows?{query}"
            )
            if any(
                title in (self.snapshot_title(entry.get("snapshot", {})) or "")
                for entry in listed.get("windows", [])
            ):
                break
            time.sleep(0.1)
        else:
            raise AcceptanceError("window list did not contain the expected fixture")

        request = {
            "desktop_id": self.desktop_id,
            "desktop_generation": self.generation,
            "selector": self.selector(title),
            "limit": 10,
            "order": "creation_ascending",
        }
        queried = self.json_request(
            "POST", f"/v1/desktops/{self.desktop_id}/windows/query", request
        )
        if len(queried.get("windows", [])) != 1:
            raise AcceptanceError("window query was not an exact singleton")

        request.pop("limit")
        request["match_policy"] = "exactly_one"
        resolved = self.json_request(
            "POST", f"/v1/desktops/{self.desktop_id}/windows/resolve", request
        )
        entry = resolved.get("window", {})
        snapshot = entry.get("snapshot", {})
        reference = snapshot.get("ref")
        token = entry.get("reference_token")
        resolved_title = self.snapshot_title(snapshot)
        if (
            not isinstance(resolved_title, str)
            or title not in resolved_title
            or not isinstance(reference, dict)
        ):
            raise AcceptanceError("window resolve returned mismatched evidence")
        if not isinstance(token, str):
            raise AcceptanceError("window resolve omitted reference token")
        snapshot_query = urllib.parse.urlencode({"desktop_generation": self.generation})
        exact = self.json_request(
            "GET",
            f"/v1/desktops/{self.desktop_id}/windows/"
            f"{urllib.parse.quote(token, safe='')}?{snapshot_query}",
        )
        if (
            self.snapshot_title(exact.get("window", {}).get("snapshot", {}))
            != resolved_title
        ):
            raise AcceptanceError("token snapshot did not preserve exact window identity")
        return reference

    def activate(self, window: dict[str, Any]) -> None:
        result = self.submit(
            {
                "type": "window_activate",
                "window": window,
                "switch_workspace": True,
                "fallback": "ewmh_only",
            }
        )
        outcome = result.get("outcome", {})
        operation = outcome.get("result", {})
        if outcome.get("type") != "window_control" or operation.get("type") != "activated":
            raise AcceptanceError("window activation returned mismatched outcome")
        if operation.get("converged") is not True:
            raise AcceptanceError("xfwm4 activation did not converge")

    def exercise_window_manager(self, window: dict[str, Any]) -> None:
        clamped = self.submit(
            {
                "type": "window_move_resize",
                "window": window,
                "relative_to": "frame",
                "geometry": {"x": -200, "y": -100, "width": 1000, "height": 800},
                "bounds_policy": "clamp_to_root",
            }
        ).get("outcome", {}).get("result", {})
        clamped_rect = clamped.get("effective", {}).get("rect", {})
        if (
            clamped.get("type") != "geometry_changed"
            or clamped.get("converged") is not True
            or clamped.get("constrained") is not True
            or (clamped_rect.get("x"), clamped_rect.get("y")) != (0, 0)
        ):
            raise AcceptanceError(
                "xfwm4 clamped move/resize did not converge: "
                + json.dumps(clamped, sort_keys=True)[:1200]
            )

        result = self.submit(
            {
                "type": "window_move_resize",
                "window": window,
                "relative_to": "frame",
                "geometry": {"x": 120, "y": 90, "width": 1000, "height": 800},
                "bounds_policy": "require_inside_root",
            }
        )
        operation = result.get("outcome", {}).get("result", {})
        if operation.get("type") != "geometry_changed" or operation.get("converged") is not True:
            raise AcceptanceError(
                "xfwm4 move/resize did not converge: "
                + json.dumps(operation, sort_keys=True)[:1200]
            )
    def minimize(self, window: dict[str, Any]) -> None:
        result = self.submit(
            {
                "type": "window_minimize",
                "window": window,
                "desired": True,
            }
        )
        operation = result.get("outcome", {}).get("result", {})
        if operation.get("type") != "minimized" or operation.get("converged") is not True:
            raise AcceptanceError(
                "xfwm4 minimize=True transition did not converge: "
                + json.dumps(operation, sort_keys=True)[:1200]
            )

    def clipboard_read(self) -> dict[str, Any]:
        query = urllib.parse.urlencode({"desktop_generation": self.generation})
        return self.json_request(
            "POST",
            f"/v1/desktops/{self.desktop_id}/clipboard/read?{query}",
            {
                "selection": "clipboard",
                "preferred_targets": ["UTF8_STRING"],
                "allow_binary_fallback": False,
            },
        )

    def selection_set(self, content: dict[str, Any]) -> None:
        result = self.submit(
            {"type": "selection_set", "selection": "clipboard", "content": content}
        )
        if result.get("outcome") != {"type": "acknowledged"}:
            raise AcceptanceError("selection_set did not return acknowledged outcome")

    def selection_clear(self) -> None:
        result = self.submit({"type": "selection_clear", "selection": "clipboard"})
        if result.get("outcome") != {"type": "acknowledged"}:
            raise AcceptanceError("selection_clear did not return acknowledged outcome")

    def upload(self, body: bytes) -> dict[str, Any]:
        digest = hashlib.sha256(body).hexdigest()
        _, response_headers, payload = self.request(
            "POST",
            "/v1/artifacts?purpose=clipboard_input",
            body=body,
            content_type="text/plain;charset=utf-8",
            headers={"x-content-sha256": digest},
            expected=(201,),
        )
        if response_headers.get("Location") is None:
            raise AcceptanceError("artifact upload omitted Location")
        artifact = json.loads(payload)
        if (
            artifact.get("purpose") != "clipboard_input"
            or artifact.get("content_length") != len(body)
            or artifact.get("sha256") != digest
        ):
            raise AcceptanceError("artifact upload metadata mismatched exact input")
        return artifact

    def artifact_path(self, artifact: dict[str, Any]) -> str:
        query = urllib.parse.urlencode(
            {"desktop_id": self.desktop_id, "desktop_generation": self.generation}
        )
        return f"/v1/artifacts/{artifact['artifact_id']}?{query}"

    def download_artifact(self, artifact: dict[str, Any], expected: bytes) -> None:
        path = self.artifact_path(artifact)
        _, range_headers, prefix = self.request(
            "GET", path, headers={"Range": "bytes=0-63"}, expected=(206,)
        )
        expected_prefix = expected[:64]
        if prefix != expected_prefix or range_headers.get("Content-Range") != (
            f"bytes 0-{len(expected_prefix) - 1}/{len(expected)}"
        ):
            raise AcceptanceError("artifact range response mismatched immutable body")
        _, headers, body = self.request("GET", path)
        digest = hashlib.sha256(body).hexdigest()
        if body != expected or headers.get("x-content-sha256") != digest:
            raise AcceptanceError("artifact download mismatched exact bytes or digest header")

    def delete_artifact(self, artifact: dict[str, Any]) -> None:
        self.request("DELETE", self.artifact_path(artifact), expected=(204,))

    @staticmethod
    def screenshot_body(
        target: dict[str, Any],
        *,
        include_cursor: bool = False,
        max_bytes: int | None = None,
    ) -> dict[str, Any]:
        return {
            "target": target,
            "region": None,
            "format": "png",
            "include_cursor": include_cursor,
            "scale": None,
            "max_bytes": max_bytes,
        }

    def capture(
        self,
        target: dict[str, Any],
        *,
        include_cursor: bool = False,
        max_bytes: int | None = None,
    ) -> dict[str, Any]:
        query = urllib.parse.urlencode({"desktop_generation": self.generation})
        result = self.json_request(
            "POST",
            f"/v1/desktops/{self.desktop_id}/screenshots?{query}",
            self.screenshot_body(
                target, include_cursor=include_cursor, max_bytes=max_bytes
            ),
        )
        expected_limitation = {
            "root": "root_visible_framebuffer",
            "window_visible": "window_visible_includes_occluders",
            "window_drawable": "window_drawable_obscured_undefined",
        }.get(target.get("kind"))
        if (
            result.get("target") != target
            or result.get("format") != "png"
            or result.get("limitation") != expected_limitation
        ):
            raise AcceptanceError("screenshot returned mismatched target metadata")
        cursor = result.get("cursor", {})
        if include_cursor:
            serials = (cursor.get("serial_before"), cursor.get("serial_after"))
            if (
                cursor.get("requested") is not True
                or cursor.get("composited") is not True
                or any(
                    not isinstance(serial, int)
                    or isinstance(serial, bool)
                    or serial < 0
                    for serial in serials
                )
                or not isinstance(cursor.get("moved_during_capture"), bool)
            ):
                raise AcceptanceError("cursor capture evidence was incomplete or inconsistent")
        elif cursor != {
            "requested": False,
            "composited": False,
            "serial_before": None,
            "serial_after": None,
            "moved_during_capture": False,
        }:
            raise AcceptanceError("cursor-disabled capture returned observation evidence")
        delivery = result.get("delivery", {})
        artifact = delivery.get("artifact")
        if delivery.get("delivery") != "artifact" or not isinstance(artifact, dict):
            raise AcceptanceError("screenshot did not return a private artifact")
        path = self.artifact_path(artifact)
        _, headers, body = self.request("GET", path)
        if len(body) < 24 or body[:8] != b"\x89PNG\r\n\x1a\n":
            raise AcceptanceError("screenshot artifact is not a bounded PNG")
        width, height = struct.unpack(">II", body[16:24])
        digest = hashlib.sha256(body).hexdigest()
        if (
            width == 0
            or height == 0
            or digest != result.get("sha256")
            or artifact.get("purpose") != "screenshot"
            or artifact.get("content_type") != "image/png"
            or artifact.get("content_length") != len(body)
            or artifact.get("sha256") != digest
        ):
            raise AcceptanceError("screenshot PNG geometry or digest is invalid")
        if headers.get("x-content-sha256") != digest:
            raise AcceptanceError("screenshot artifact digest header mismatched")
        self.delete_artifact(artifact)
        return result

    def capture_rejected(
        self,
        target: dict[str, Any],
        *,
        include_cursor: bool = False,
        max_bytes: int | None = None,
        expected_status: int,
        expected_code: str,
    ) -> None:
        query = urllib.parse.urlencode({"desktop_generation": self.generation})
        problem = self.json_request(
            "POST",
            f"/v1/desktops/{self.desktop_id}/screenshots?{query}",
            self.screenshot_body(
                target, include_cursor=include_cursor, max_bytes=max_bytes
            ),
            expected=(expected_status,),
        )
        if (
            problem.get("status") != expected_status
            or problem.get("code") != expected_code
            or problem.get("retry") != "never"
            or problem.get("effect_stage") != "none"
            or problem.get("details") != {}
            or any(key in problem for key in ("delivery", "artifact", "sha256"))
        ):
            safe_shape = {
                key: problem.get(key)
                for key in ("status", "code", "retry", "effect_stage")
            }
            raise AcceptanceError(
                "screenshot rejection evidence mismatched: "
                + json.dumps(safe_shape, sort_keys=True)
            )

    def text_insert(
        self,
        window: dict[str, Any],
        text_source: dict[str, Any],
        expected_bytes: int,
        expected_mode: str,
    ) -> list[str]:
        result = self.submit(
            {
                "type": "text_insert",
                "text": text_source,
                "target": {"target": "window", "window": window},
                "strategy": "clipboard",
                "clipboard_options": {
                    "preserve_clipboard": True,
                    "paste_observation_timeout_ms": 2000,
                },
            },
            lease=True,
            timeout=120,
        )
        outcome = result.get("outcome", {})
        evidence = outcome.get("evidence", {})
        clipboard = evidence.get("clipboard", {})
        transfer = clipboard.get("transfer") or {}
        restoration = clipboard.get("restoration", {})
        requested_targets = clipboard.get("requested_targets")
        if (
            outcome.get("type") != "text_inserted"
            or evidence.get("selected_strategy") != "clipboard"
            or evidence.get("utf8_bytes") != expected_bytes
            or evidence.get("completed_scalars") != expected_bytes
            or clipboard.get("request_observed") is not True
            or transfer.get("transfer", {}).get("mode") != expected_mode
            or transfer.get("terminal", {}).get("status") != "completed"
            or restoration.get("requested") is not True
            or restoration.get("previous_owner_existed") is not True
            # Xenoteer can restore the bounded text value exactly, but cannot
            # restore the previous owner's identity or arbitrary non-text
            # targets. The protocol deliberately reports that honest scope.
            or restoration.get("kind") != "partial_value_copy"
            or not isinstance(requested_targets, list)
            or not all(isinstance(target, str) for target in requested_targets)
        ):
            safe_evidence = {
                "outcome_type": outcome.get("type"),
                "selected_strategy": evidence.get("selected_strategy"),
                "utf8_bytes": evidence.get("utf8_bytes"),
                "completed_scalars": evidence.get("completed_scalars"),
                "request_observed": clipboard.get("request_observed"),
                "transfer_mode": transfer.get("transfer", {}).get("mode"),
                "transfer_status": transfer.get("terminal", {}).get("status"),
                "restoration_requested": restoration.get("requested"),
                "previous_owner_existed": restoration.get("previous_owner_existed"),
                "restoration_kind": restoration.get("kind"),
                "requested_targets": requested_targets,
            }
            raise AcceptanceError(
                "text insertion/paste/restoration evidence did not converge: "
                + json.dumps(safe_evidence, sort_keys=True)
            )
        return requested_targets


def wait_public_ready(base_url: str, container: LiveContainer) -> None:
    for _ in range(120):
        try:
            response = urllib.request.urlopen(base_url + "/readyz", timeout=2)
            with response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
        running = docker(
            "inspect", container.name, "--format", "{{.State.Running}}", timeout=4
        ).stdout.strip()
        if running != "true":
            raise AcceptanceError("container stopped before API readiness")
        time.sleep(0.5)
    raise AcceptanceError("Phase-4 API did not become ready")


def write_inputs(directory: Path) -> dict[str, Path]:
    inputs = {
        "direct-owner": directory / "direct-owner.txt",
        "incr-owner": directory / "incr-owner.txt",
        "set-direct": directory / "set-direct.txt",
        "restore": directory / "restore.txt",
        "incr-source": directory / "incr-source.txt",
    }
    inputs["direct-owner"].write_bytes(b"phase4-direct-owner")
    inputs["incr-owner"].write_bytes(b"phase4-incr-owner:" + b"r" * INCR_BYTES)
    inputs["set-direct"].write_bytes(b"phase4-direct-set")
    inputs["restore"].write_bytes(b"phase4-restore-canary")
    inputs["incr-source"].write_bytes(b"phase4-incr-source:" + b"p" * INCR_BYTES)
    return inputs


def verify_clipboard_read(
    api: ApiClient,
    expected: bytes,
    expected_mode: str,
) -> None:
    result = api.clipboard_read()
    evidence = result.get("evidence", {})
    if (
        evidence.get("transfer", {}).get("mode") != expected_mode
        or evidence.get("terminal", {}).get("status") != "completed"
        or evidence.get("content_length") != len(expected)
        or evidence.get("sha256") != hashlib.sha256(expected).hexdigest()
    ):
        raise AcceptanceError("clipboard read transfer evidence mismatched")
    delivery = result.get("content", {})
    if expected_mode == "direct":
        if delivery != {"delivery": "inline_text", "text": expected.decode("utf-8")}:
            raise AcceptanceError("direct clipboard read was not exact inline text")
    else:
        artifact = delivery.get("artifact")
        if delivery.get("delivery") != "artifact" or not isinstance(artifact, dict):
            raise AcceptanceError("INCR clipboard read did not return an artifact")
        api.download_artifact(artifact, expected)
        api.delete_artifact(artifact)


def verify_capture_rejection_without_artifact(
    container: LiveContainer,
    api: ApiClient,
    target: dict[str, Any],
    *,
    include_cursor: bool = False,
    max_bytes: int | None = None,
    expected_status: int,
    expected_code: str,
) -> None:
    before = container.artifact_inventory()
    api.capture_rejected(
        target,
        include_cursor=include_cursor,
        max_bytes=max_bytes,
        expected_status=expected_status,
        expected_code=expected_code,
    )
    after = container.artifact_inventory()
    if after != before:
        raise AcceptanceError(
            "rejected screenshot changed private artifact counts: "
            f"durable={before[0]}->{after[0]}, temporary={before[1]}->{after[1]}"
        )


def ensure_host_requirements() -> None:
    for command in ("docker",):
        if shutil.which(command) is None:
            raise AcceptanceError(f"required host command is unavailable: {command}")
    if os.geteuid() != 0:
        security = docker("info", "--format", "{{json .SecurityOptions}}").stdout
        if "rootless" not in security:
            raise AcceptanceError(
                "run as root or use rootless Docker so the token bind mount has safe ownership"
            )


def verify_image(image_reference: str) -> str:
    image = docker("image", "inspect", image_reference, "--format", "{{.Id}}").stdout.strip()
    if not image.startswith("sha256:") or len(image) != 71:
        raise AcceptanceError("desktop-app fixture did not resolve to an immutable image ID")
    label = docker(
        "image",
        "inspect",
        image,
        "--format",
        '{{index .Config.Labels "com.aeor.xenoteer.fixture"}}',
    ).stdout.strip()
    if label != "phase-2-desktop-apps":
        raise AcceptanceError("image is not the expected desktop-application fixture layer")
    return image


def main() -> int:
    if len(sys.argv) > 2:
        raise AcceptanceError("usage: test-phase4-live-fixtures.py [DESKTOP_APPS_IMAGE]")
    image_reference = (
        sys.argv[1]
        if len(sys.argv) == 2
        else os.environ.get("XENOTEER_DESKTOP_APPS_IMAGE", "xenoteer:desktop-apps-test")
    )
    ensure_host_requirements()
    image = verify_image(image_reference)
    daemon_override_value = os.environ.get("XENOTEERD_BINARY_OVERRIDE")
    daemon_override = (
        Path(daemon_override_value).resolve()
        if daemon_override_value is not None
        else None
    )
    if daemon_override is not None and not (
        daemon_override.is_file() and os.access(daemon_override, os.X_OK)
    ):
        raise AcceptanceError(
            "XENOTEERD_BINARY_OVERRIDE must name an executable regular file"
        )

    with tempfile.TemporaryDirectory(prefix="xenoteer-phase4-live-") as temporary:
        directory = Path(temporary)
        token_file = directory / "api-token"
        token_file.write_bytes(TOKEN_CANARY)
        token_file.chmod(0o400)
        if os.geteuid() == 0 and daemon_override is not None:
            # The stale diagnostic image predates the daemon UID split and its
            # initializer requires the source token to be owned by UID 1000.
            # Current images deliberately require root ownership instead.
            os.chown(token_file, 1000, 1000)
        inputs = write_inputs(directory)
        container = LiveContainer(image, token_file, daemon_override)
        try:
            container.start()
            container.wait_ready()
            port = container.api_port()
            base_url = f"http://127.0.0.1:{port}"
            wait_public_ready(base_url, container)
            container.require_fixtures()

            api = ApiClient(base_url, TOKEN_CANARY)
            api.discover()

            container_paths: dict[str, str] = {}
            for name, source in inputs.items():
                destination = f"/run/user/1000/phase4-{name}.txt"
                container.copy_fixture_file(source, destination)
                container_paths[name] = destination

            container.start_clipboard_owner(
                container_paths["direct-owner"],
                "/run/user/1000/phase4-direct-owner.ready",
            )
            verify_clipboard_read(api, inputs["direct-owner"].read_bytes(), "direct")
            container.stop_clipboard_owner()

            container.start_clipboard_owner(
                container_paths["incr-owner"],
                "/run/user/1000/phase4-incr-owner.ready",
            )
            verify_clipboard_read(api, inputs["incr-owner"].read_bytes(), "incr")
            container.stop_clipboard_owner()

            direct_set = inputs["set-direct"].read_text(encoding="utf-8")
            api.selection_set({"source": "inline_text", "text": direct_set})
            container.verify_clipboard(container_paths["set-direct"])
            api.selection_clear()

            incr_body = inputs["incr-source"].read_bytes()
            incr_artifact = api.upload(incr_body)
            api.selection_set(
                {
                    "source": "artifact",
                    "artifact": incr_artifact,
                    "target": "UTF8_STRING",
                }
            )
            container.verify_clipboard(container_paths["incr-source"])
            api.selection_clear()

            root_target = {"kind": "root"}
            for max_bytes in (0, MAX_SCREENSHOT_BYTES + 1, 1):
                verify_capture_rejection_without_artifact(
                    container,
                    api,
                    root_target,
                    max_bytes=max_bytes,
                    expected_status=400,
                    expected_code="invalid_request",
                )
            api.capture(root_target, include_cursor=True)

            for fixture in APPLICATIONS:
                container.launch(fixture)
                try:
                    window = api.resolve_window(fixture.title)
                    api.activate(window)
                    if fixture.name == "gtk3":
                        api.exercise_window_manager(window)
                        api.activate(window)
                    container.focus_entry(fixture.entry_name)
                    initial = directory / f"initial-{fixture.name}.txt"
                    initial.write_bytes(fixture.initial_text)
                    container.copy_fixture_file(
                        initial, f"/run/user/1000/phase4-initial-{fixture.name}.txt"
                    )
                    container.verify_entry(
                        fixture.entry_name,
                        f"/run/user/1000/phase4-initial-{fixture.name}.txt",
                    )

                    if fixture.name == "qtwebengine":
                        print(
                            json.dumps(
                                {
                                    "type": "qtwebengine_clipboard_limitation",
                                    "exact_insert_skipped": True,
                                    "reason": "forced_accessibility_duplicate_paste",
                                },
                                sort_keys=True,
                            ),
                            flush=True,
                        )
                    else:
                        container.start_clipboard_owner(
                            container_paths["restore"],
                            f"/run/user/1000/phase4-{fixture.name}-restore.ready",
                        )
                        if fixture.name == "gtk3":
                            inserted = incr_body
                            source = {"source": "artifact", "artifact": incr_artifact}
                            mode = "incr"
                        else:
                            inserted = f"-phase4-{fixture.name}".encode("utf-8")
                            source = {
                                "source": "inline",
                                "text": inserted.decode("utf-8"),
                            }
                            mode = "direct"
                        expected = directory / f"expected-{fixture.name}.txt"
                        expected.write_bytes(fixture.initial_text + inserted)
                        container.copy_fixture_file(
                            expected,
                            f"/run/user/1000/phase4-expected-{fixture.name}.txt",
                        )
                        api.ensure_lease()
                        requested_targets = api.text_insert(
                            window, source, len(inserted), mode
                        )
                        container.stop_clipboard_owner()
                        try:
                            container.verify_entry(
                                fixture.entry_name,
                                f"/run/user/1000/phase4-expected-{fixture.name}.txt",
                            )
                        except AcceptanceError as error:
                            raise AcceptanceError(
                                f"{fixture.name} editable postcondition failed after targets "
                                f"{requested_targets!r}: {error}"
                            ) from error
                        restored = api.clipboard_read()
                        if restored.get("content") != {
                            "delivery": "inline_text",
                            "text": inputs["restore"].read_text(encoding="utf-8"),
                        }:
                            raise AcceptanceError(
                                "clipboard value-copy restoration was not exact"
                            )
                        api.selection_clear()

                    api.capture(
                        {
                            "kind": "window_visible",
                            "window": window,
                            "coordinate_space": "client",
                        }
                    )
                    if fixture.name == "chromium":
                        # Exercise iconification only after all focus-sensitive
                        # fixture work, using a top-level without the deliberately
                        # mapped GTK/Qt transient-dialog constraint.
                        api.minimize(window)
                        verify_capture_rejection_without_artifact(
                            container,
                            api,
                            {"kind": "window_drawable", "window": window},
                            expected_status=422,
                            expected_code="unsupported_by_target",
                        )
                finally:
                    container.stop_clipboard_owner()
                    container.stop_fixture(fixture)

            api.delete_artifact(incr_artifact)
            print(
                json.dumps(
                    {
                        "type": "phase4_live_fixture_acceptance",
                        "applications": [fixture.name for fixture in APPLICATIONS],
                        "clipboard_modes": ["direct", "incr"],
                        "root_capture": True,
                        "cursor_capture": True,
                        "capture_limit_rejections": 3,
                        "minimized_capture_rejection": True,
                        "window_capture_count": len(APPLICATIONS),
                        "container_cpus": 2,
                    },
                    sort_keys=True,
                )
            )
        except Exception:
            logs = container.safe_logs()
            if logs:
                print("--- sanitized Phase-4 fixture logs ---", file=sys.stderr)
                print(logs, file=sys.stderr)
            raise
        finally:
            container.remove()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AcceptanceError as error:
        print(f"Phase-4 live fixture acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
