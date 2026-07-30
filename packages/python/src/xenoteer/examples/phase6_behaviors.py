#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Qualify ten public SDK behaviors against the deterministic desktop fixture.

Run this installed-package example with the environment documented in the
package README. The Phase 6 artifact gate supplies those values and launches
the GTK fixture from the exact derived image before invoking this module.
"""

from __future__ import annotations

import asyncio
import hashlib
import os
import re
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any, cast

import xenoteer
from xenoteer import ArtifactRef, ClientOptions, XenoteerClient, XenoteerError


BEHAVIORS = (
    "status-capabilities",
    "scoped-lease-fixture-launch",
    "exact-window-element",
    "semantic-invoke",
    "smooth-physical-click-postcondition",
    "unicode-text-strategy",
    "screenshot-on-failure",
    "reconnect-known-command",
    "stale-reference-restart",
    "view-only-browser-ticket",
)
GTK_TITLE = "Xenoteer GTK3 Fixture — Main"
XMESSAGE_TITLE = "xmessage"
VIEWER_ORIGIN = "https://viewer.example"
UNICODE_TEXT = "Xenoteer — العربية — 中文 — e\u0301 — 😀"


def required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"required environment is missing: {name}")
    return value


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def verify_installed_origin() -> None:
    expected = Path(required("XENOTEER_EXPECTED_INSTALL_ROOT")).resolve(strict=True)
    module_file = Path(xenoteer.__file__).resolve(strict=True)
    require(
        module_file.is_relative_to(expected),
        "Python SDK resolved outside the staged distribution installation",
    )


def marker(language: str, behavior: str) -> None:
    print(f"quickstart-ok language={language} behavior={behavior}")


def window_selector(title: str) -> dict[str, Any]:
    return {
        "type": "predicate",
        "predicate": {
            "type": "text",
            "field": "title",
            "matcher": {
                "type": "exact",
                "value": title,
                "case_sensitive": True,
            },
        },
    }


def element_selector(name: str) -> dict[str, Any]:
    return {
        "scope": {"type": "desktop"},
        "predicates": [
            {
                "type": "name",
                "matcher": {
                    "type": "exact",
                    "value": name,
                    "case_sensitive": True,
                },
            }
        ],
        "order": "object_path_ascending",
        "result_index": None,
    }


def accessibility_options(*, component: bool = False) -> dict[str, Any]:
    return {
        "expansion": {
            "actions": False,
            "value": False,
            "text_metadata": False,
            "text_content": False,
            "attributes": False,
            "relations": False,
            "component": component,
        },
        "limits": {
            "max_visited_nodes": 25_000,
            "max_depth": 64,
            "max_matches": 10,
            "timeout_ms": 10_000,
        },
    }


async def wait_element(desktop: Any, name: str) -> Any:
    selector = element_selector(name)
    await desktop.accessibility.wait(
        {
            "target": {
                "type": "selector",
                "selector": selector,
                "quantifier": "exactly_one",
            },
            "predicate": {"type": "exists"},
            "after_revision": None,
            "timeout_ms": 30_000,
            "allow_poll_fallback": True,
            **accessibility_options(component=False),
        }
    )
    return await desktop.accessibility.one(
        selector,
        **accessibility_options(component=True),
    )


async def wait_window(desktop: Any, title: str) -> Any:
    selector = window_selector(title)
    await desktop.windows.wait(
        {
            "target": {
                "type": "selector",
                "selector": selector,
                "quantifier": "exactly_one",
            },
            "predicate": {"type": "exists"},
            "after_revision": None,
            "timeout_ms": 30_000,
        }
    )
    return await desktop.windows.one(selector, order="creation_ascending")


async def terminal(submission: Any, label: str) -> dict[str, Any]:
    handle = await submission
    result = await handle.wait_until_terminal(20)
    if not isinstance(result, dict) or any(not isinstance(key, str) for key in result):
        raise RuntimeError(f"{label} returned an invalid terminal result")
    typed_result = cast(dict[str, Any], result)
    require(
        typed_result.get("lifecycle") == "succeeded",
        f"{label} did not succeed",
    )
    return typed_result


def outcome(result: Mapping[str, Any], kind: str) -> Mapping[str, Any]:
    value = result.get("outcome")
    if not isinstance(value, Mapping) or value.get("type") != kind:
        raise RuntimeError(f"command omitted its {kind} outcome")
    return value


async def launch_xmessage(desktop: Any, message: str) -> Mapping[str, Any]:
    result = await terminal(
        desktop.applications.launch(
            "xmessage",
            [message],
        ),
        "xmessage launch",
    )
    process = outcome(result, "application_launched").get("process")
    if not isinstance(process, Mapping):
        raise RuntimeError("launch outcome omitted its exact process")
    return process


async def terminate_process(desktop: Any, process: Mapping[str, Any]) -> None:
    result = await terminal(
        desktop.applications.terminate(process, grace=2),
        "fixture termination",
    )
    terminated = outcome(result, "process_terminated").get("process")
    require(
        isinstance(terminated, Mapping) and terminated.get("state") == "exited",
        "fixture termination did not reap the exact process",
    )


async def exercise_connected(client: XenoteerClient, language: str) -> None:
    desktop = client.desktop()
    capabilities = client.status.capabilities.get("capabilities")
    if not isinstance(capabilities, list):
        raise RuntimeError("status omitted capabilities")
    available = {
        item.get("id")
        for item in capabilities
        if isinstance(item, Mapping) and item.get("status") == "available"
    }
    required_capabilities = {
        "accessibility.atspi",
        "capture.screenshot",
        "input.pointer.smooth",
        "process.managed.terminate",
        "viewer.novnc.view_only",
        "window.observe.wait",
    }
    require(
        required_capabilities <= available,
        "fixture did not advertise every behavior capability as available",
    )
    marker(language, BEHAVIORS[0])

    async with desktop.control(ttl=60) as lease:
        await exercise_scoped(desktop, language, lease)


async def exercise_scoped(desktop: Any, language: str, lease: Any) -> None:
    message = f"Xenoteer SDK Phase 6 — {language}"
    process: Mapping[str, Any] | None = None
    screenshot_artifact: ArtifactRef | None = None
    operation_failure: BaseException | None = None
    try:
        process = await launch_xmessage(desktop, message)
        marker(language, BEHAVIORS[1])

        xmessage_window = await wait_window(desktop, XMESSAGE_TITLE)
        gtk_window = await wait_window(desktop, GTK_TITLE)
        button = await wait_element(desktop, "Stable Button")
        entry = await wait_element(desktop, "Stable Entry")
        require(xmessage_window.identity != gtk_window.identity, "exact windows aliased")
        require(button.identity != entry.identity, "exact elements aliased")
        correlated = await xmessage_window.snapshot()
        require(
            correlated.get("window", {})
            .get("snapshot", {})
            .get("process", {})
            .get("managed_process")
            == process,
            "exact xmessage window did not correlate to the launched process",
        )
        marker(language, BEHAVIORS[2])

        invoke_result = await terminal(button.invoke(), "semantic invoke")
        invoke_outcome = outcome(invoke_result, "element_action")
        invoke_evidence = invoke_outcome.get("result")
        require(
            isinstance(invoke_evidence, Mapping)
            and invoke_evidence.get("operation") == "invoke"
            and invoke_evidence.get("element") == button.identity
            and isinstance(invoke_evidence.get("evidence"), Mapping)
            and invoke_evidence["evidence"].get("backend_accepted") is True,
            "semantic invoke omitted exact-target actor-owned evidence",
        )
        await wait_element(desktop, "Activation Count 1")
        marker(language, BEHAVIORS[3])

        click_result = await terminal(
            button.physical_click(
                lease,
                window=gtk_window,
                move_duration=0.25,
                postcondition={
                    "predicate": {"type": "exists"},
                    "timeout_ms": 3_000,
                    "allow_poll_fallback": True,
                },
            ),
            "smooth physical click",
        )
        click_outcome = outcome(click_result, "element_physical_click")
        click_evidence = click_outcome.get("result")
        require(
            isinstance(click_evidence, Mapping)
            and click_evidence.get("pointer_interpolated") is True,
            "physical click did not report interpolated pointer motion",
        )
        await wait_element(desktop, "Activation Count 2")
        marker(language, BEHAVIORS[4])

        await terminal(entry.set_text(""), "entry reset")
        text_result = await terminal(
            lease.keyboard.insert_text(
                UNICODE_TEXT,
                {
                    "target": "element",
                    "element": entry.ref,
                    "window_fallback": None,
                },
                strategy="auto",
                verify_length_only=False,
            ),
            "Unicode insertion",
        )
        text_evidence = outcome(text_result, "text_inserted").get("evidence")
        require(
            isinstance(text_evidence, Mapping)
            and text_evidence.get("selected_strategy") == "semantic"
            and text_evidence.get("utf8_bytes") == len(UNICODE_TEXT.encode("utf-8"))
            and text_evidence.get("unicode_scalars") == len(UNICODE_TEXT)
            and text_evidence.get("completed_scalars") == len(UNICODE_TEXT)
            and text_evidence.get("verified_length_only") is False,
            "Unicode insertion omitted exact delivery and strategy evidence",
        )
        marker(language, BEHAVIORS[5])

        failed = await button.invoke(
            postcondition={
                "predicate": {
                    "type": "state",
                    "state": "checked",
                    "value": True,
                },
                "timeout_ms": 750,
                "allow_poll_fallback": True,
            }
        )
        try:
            await failed.wait_until_terminal(10)
        except XenoteerError:
            pass
        else:
            raise RuntimeError("deliberately impossible postcondition unexpectedly succeeded")
        failed_result = failed.latest
        require(
            failed_result.get("lifecycle") == "failed"
            and failed_result.get("effect_stage")
            in {
                "semantic_action_dispatched",
                "semantic_state_changed",
            },
            "failed postcondition omitted visible-effect evidence",
        )
        await wait_element(desktop, "Activation Count 3")
        screenshot = await desktop.capture.screenshot(
            target={
                "kind": "window_visible",
                "window": gtk_window.ref,
                "coordinate_space": "frame",
            },
            include_cursor=True,
            max_bytes=4 * 1_048_576,
        )
        delivery = screenshot.get("delivery")
        require(
            isinstance(delivery, Mapping) and delivery.get("delivery") == "artifact",
            "failure screenshot was not retained as a private artifact",
        )
        screenshot_artifact = ArtifactRef.from_wire(
            delivery.get("artifact"),
            desktop_id=desktop.id,
            desktop_generation=desktop.generation,
            purpose="screenshot",
        )
        screenshot_bytes = await desktop.artifacts.download_bytes(screenshot_artifact)
        require(
            screenshot_bytes.startswith(b"\x89PNG\r\n\x1a\n")
            and hashlib.sha256(screenshot_bytes).hexdigest() == screenshot_artifact.sha256
            and screenshot.get("sha256") == screenshot_artifact.sha256,
            "failure screenshot bytes did not match their artifact evidence",
        )
        await desktop.artifacts.delete(screenshot_artifact)
        screenshot_artifact = None
        marker(language, BEHAVIORS[6])

        probe = desktop.submit({"type": "desktop_probe"})
        known_command_id = probe.id
        probe_result = await terminal(probe, "known-ID probe")
        require(outcome(probe_result, "probe").get("ready") is True, "probe was not ready")
        reconnect = await XenoteerClient.connect(
            ClientOptions(
                base_url=required("XENOTEER_API_BASE"),
                token=required("XENOTEER_TOKEN"),
                request_timeout=5,
            )
        )
        async with reconnect:
            recovered = await reconnect.desktop().command(known_command_id)
            recovered_result = await recovered.wait_until_terminal(10)
            require(
                recovered_result.get("command_id") == known_command_id
                and recovered_result.get("lifecycle") == "succeeded",
                "reconnect did not recover the known command ID",
            )
        marker(language, BEHAVIORS[7])

        old_window = xmessage_window
        old_identity = old_window.identity
        await terminate_process(desktop, process)
        process = None
        await desktop.windows.wait(
            {
                "target": {"type": "reference", "window": old_identity},
                "predicate": {"type": "gone"},
                "after_revision": None,
                "timeout_ms": 15_000,
            }
        )
        try:
            await old_window.snapshot()
        except XenoteerError as error:
            require(
                error.code == "stale_reference" or error.problem_code == "stale_reference",
                "old window failed with a non-stale error",
            )
        else:
            raise RuntimeError("old window reference remained valid after restart")
        process = await launch_xmessage(desktop, message)
        new_window = await wait_window(desktop, XMESSAGE_TITLE)
        require(new_window.identity != old_identity, "restart reused the exact window birth")
        marker(language, BEHAVIORS[8])

        ticket = await desktop.viewer.ticket(VIEWER_ORIGIN)
        metadata = ticket.metadata
        secret = ticket.expose_ticket()
        require(
            metadata.get("origin") == VIEWER_ORIGIN
            and metadata.get("mode") == "view_only"
            and metadata.get("audience") == "viewer_websocket"
            and metadata.get("use_policy") == "single_use"
            and 43 <= len(secret) <= 128
            and secret not in repr(ticket),
            "browser ticket was not exact-origin, single-use, and view-only",
        )
        marker(language, BEHAVIORS[9])
    except BaseException as error:
        operation_failure = error
    finally:
        failures = [] if operation_failure is None else [operation_failure]
        if screenshot_artifact is not None:
            try:
                await desktop.artifacts.delete(screenshot_artifact)
            except BaseException as error:
                failures.append(error)
        if process is not None:
            try:
                await terminate_process(desktop, process)
            except BaseException as error:
                failures.append(error)
        if len(failures) == 1:
            raise failures[0]
        if failures:
            raise BaseExceptionGroup(
                "behavior execution and resource cleanup failed",
                failures,
            )


async def exercise() -> None:
    verify_installed_origin()
    language = required("XENOTEER_QUICKSTART_LANGUAGE")
    require(
        re.fullmatch(r"[a-z-]+", language) is not None,
        "quick-start language label is invalid",
    )
    expect_authentication_failure = required("XENOTEER_EXPECT_AUTH_FAILURE") == "1"
    options = ClientOptions(
        base_url=required("XENOTEER_API_BASE"),
        token=required("XENOTEER_TOKEN"),
        request_timeout=5,
    )
    try:
        client = await XenoteerClient.connect(options)
    except XenoteerError as error:
        if expect_authentication_failure and error.code == "authentication" and error.status == 401:
            print(f"quickstart-ok language={language} mode=auth-failure")
            return
        raise
    async with client:
        require(
            not expect_authentication_failure,
            "invalid bearer unexpectedly authenticated",
        )
        require(
            client.negotiated_protocol.major == 1
            and client.negotiated_protocol.minor == 0
            and client.status.desktop.state == "ready",
            "server did not expose a ready frozen v1.0 desktop",
        )
        await exercise_connected(client, language)
        print(f"quickstart-ok language={language} mode=success")


def main() -> int:
    try:
        asyncio.run(asyncio.wait_for(exercise(), timeout=110))
    except XenoteerError as error:
        print(
            f"public Python behavior example failed: XenoteerError[{error.code}]",
            file=sys.stderr,
        )
        return 1
    except BaseExceptionGroup:
        print(
            "public Python behavior example failed: multiple bounded failures",
            file=sys.stderr,
        )
        return 1
    except (OSError, RuntimeError, asyncio.TimeoutError) as error:
        print(f"public Python behavior example failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
