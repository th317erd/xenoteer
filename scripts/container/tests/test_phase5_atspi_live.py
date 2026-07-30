#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Network-free regression tests for the Phase 5 AT-SPI live harness."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import sys
import unittest
from collections.abc import Iterable
from pathlib import Path
from typing import Any
from unittest import mock


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
MODULE_PATH = REPOSITORY_ROOT / "scripts/container/test-phase5-atspi-live.py"
SPEC = importlib.util.spec_from_file_location(
    "test_phase5_atspi_live_module", MODULE_PATH
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load the Phase 5 AT-SPI live harness")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

DESKTOP_ID = "018f3e58-78c0-7d8e-a701-3a6ca29a0001"
DESKTOP_GENERATION = "018f3e58-78c0-7d8e-a701-3a6ca29a0002"
OTHER_DESKTOP_ID = "018f3e58-78c0-7d8e-a701-3a6ca29a0003"
OTHER_DESKTOP_GENERATION = "018f3e58-78c0-7d8e-a701-3a6ca29a0004"
AT_SPI_GENERATION = "7"
SNAPSHOT_REVISION = "41"


def cursor(label: str) -> str:
    return f"{label}-0123456789abcdef"


def element_reference(
    name: str,
    *,
    desktop_id: str = DESKTOP_ID,
    desktop_generation: str = DESKTOP_GENERATION,
    atspi_generation: str = AT_SPI_GENERATION,
) -> dict[str, Any]:
    digest = hashlib.sha256(name.encode("utf-8")).hexdigest()
    sequence = str(int(digest[:8], 16) + 1)
    return {
        "desktop_id": desktop_id,
        "desktop_generation": desktop_generation,
        "atspi_generation": atspi_generation,
        "application": {
            "desktop_id": desktop_id,
            "desktop_generation": desktop_generation,
            "atspi_generation": atspi_generation,
            "unique_bus_name": ":1.42",
            "root_object_path": "/org/example/App",
            "app_instance_generation": "3",
            "identity_hash": "a" * 64,
        },
        "object_path": f"/org/example/App/{digest[:16]}",
        "object_identity_hash": digest,
        "cache_sequence": sequence,
    }


def element_page(
    names: Iterable[str],
    next_cursor: str | None,
    *,
    desktop_id: str = DESKTOP_ID,
    desktop_generation: str = DESKTOP_GENERATION,
    atspi_generation: str = AT_SPI_GENERATION,
    snapshot_revision: str = SNAPSHOT_REVISION,
    order: str = "name_ascending",
    truncated: bool = False,
    visited_nodes: int = 4_096,
) -> dict[str, Any]:
    names = list(names)
    return {
        "desktop_id": desktop_id,
        "desktop_generation": desktop_generation,
        "atspi_generation": atspi_generation,
        "snapshot_revision": snapshot_revision,
        "order": order,
        "elements": [
            {
                "snapshot": {
                    "ref": element_reference(
                        name,
                        desktop_id=desktop_id,
                        desktop_generation=desktop_generation,
                        atspi_generation=atspi_generation,
                    ),
                    "name": name,
                    "revision": snapshot_revision,
                }
            }
            for name in names
        ],
        "next_cursor": next_cursor,
        "visited_nodes": visited_nodes,
        "truncated": truncated,
        "warnings": [],
    }


def stale_problem() -> dict[str, Any]:
    return {
        "status": 409,
        "code": "stale_reference",
        "retry": "after_resync",
        "instance": "urn:xenoteer:request:018f3e58-78c0-7d8e-a701-3a6ca29a0030",
    }


def retry_problem(status: int, code: str, retry: str) -> dict[str, Any]:
    return {
        "status": status,
        "code": code,
        "retry": retry,
        "instance": "urn:xenoteer:request:018f3e58-78c0-7d8e-a701-3a6ca29a0030",
    }


class ScriptedApiClient(MODULE.ApiClient):
    def __init__(
        self,
        responses: Iterable[
            tuple[int, dict[str, Any]]
            | tuple[int, dict[str, Any], dict[str, str]]
        ],
    ) -> None:
        super().__init__("http://network-must-not-be-used.invalid", b"unit-test-token")
        self.desktop_id = DESKTOP_ID
        self.generation = DESKTOP_GENERATION
        self.responses = iter(responses)
        self.request_cursors: list[object] = []
        self.request_timeouts: list[float] = []
        self.availability_waits: list[float] = []

    def _next_response(
        self,
        body: dict[str, Any] | None,
        timeout: float,
    ) -> tuple[int, dict[str, Any], dict[str, str]]:
        if body is None:
            raise AssertionError("pagination request omitted its JSON body")
        self.request_cursors.append(body.get("cursor"))
        self.request_timeouts.append(timeout)
        try:
            response = next(self.responses)
        except StopIteration as error:
            raise AssertionError("pagination made an unexpected request") from error
        if len(response) == 2:
            status, value = response
            return status, value, {}
        return response

    def request_json(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        *,
        timeout: float = 10,
    ) -> tuple[int, dict[str, Any]]:
        del method, path
        status, value, _ = self._next_response(body, timeout)
        return status, value

    def request_json_response(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        *,
        timeout: float = 10,
    ) -> tuple[int, dict[str, Any], dict[str, str]]:
        del method, path
        return self._next_response(body, timeout)

    def wait_accessibility_available(self, *, timeout: float = 60) -> None:
        self.availability_waits.append(timeout)


class AccessibilityPaginationTests(unittest.TestCase):
    def test_api_response_preserves_only_retry_header_needed_by_pagination(
        self,
    ) -> None:
        class Headers:
            @staticmethod
            def get_content_type() -> str:
                return "application/problem+json"

            @staticmethod
            def get(name: str) -> str | None:
                return "1" if name == "Retry-After" else None

        class Response:
            status = 429
            headers = Headers()

            def __enter__(self) -> Response:
                return self

            def __exit__(self, *args: object) -> None:
                del args

            @staticmethod
            def read(_: int) -> bytes:
                return b'{"status":429,"code":"resource_exhausted"}'

        client = MODULE.ApiClient(
            "http://network-must-not-be-used.invalid",
            b"unit-test-token",
        )
        with mock.patch.object(
            MODULE.urllib.request,
            "urlopen",
            return_value=Response(),
        ) as urlopen:
            status, value, headers = client.request_json_response(
                "GET",
                "/v1/test",
                timeout=0.25,
            )

        self.assertEqual(status, 429)
        self.assertEqual(value["code"], "resource_exhausted")
        self.assertEqual(headers, {"retry-after": "1"})
        self.assertEqual(urlopen.call_args.kwargs["timeout"], 0.25)

    def test_stale_second_page_restarts_without_mixing_query_revisions(self) -> None:
        expected = [f"Phase5 Large Row {index:05d}" for index in range(4_000, 4_096)]
        client = ScriptedApiClient(
            [
                (200, element_page(expected[:25], cursor("stale-cursor-1"))),
                (409, stale_problem()),
                (
                    200,
                    element_page(
                        expected[:25],
                        cursor("fresh-cursor-1"),
                        atspi_generation="8",
                        snapshot_revision="42",
                    ),
                ),
                (
                    200,
                    element_page(
                        expected[25:50],
                        cursor("fresh-cursor-2"),
                        atspi_generation="8",
                        snapshot_revision="42",
                    ),
                ),
                (
                    200,
                    element_page(
                        expected[50:75],
                        cursor("fresh-cursor-3"),
                        atspi_generation="8",
                        snapshot_revision="42",
                    ),
                ),
                (
                    200,
                    element_page(
                        expected[75:],
                        None,
                        atspi_generation="8",
                        snapshot_revision="42",
                    ),
                ),
            ]
        )

        names, pages = client.collect_name_prefix("Phase5 Large Row 04")

        self.assertEqual(names, expected)
        self.assertEqual(pages, 4)
        self.assertEqual(
            client.request_cursors,
            [
                None,
                cursor("stale-cursor-1"),
                None,
                cursor("fresh-cursor-1"),
                cursor("fresh-cursor-2"),
                cursor("fresh-cursor-3"),
            ],
        )
        self.assertEqual(len(client.availability_waits), 1)

    def test_first_page_resync_restarts_from_a_clean_transaction(self) -> None:
        expected = ["Phase5 Large Row 04000", "Phase5 Large Row 04001"]
        client = ScriptedApiClient(
            [
                (
                    409,
                    retry_problem(
                        409, "toolkit_protocol_error", "after_resync"
                    ),
                ),
                (200, element_page(expected, None)),
            ]
        )

        self.assertEqual(
            client.collect_name_prefix("Phase5 Large Row 04"),
            (expected, 1),
        )
        self.assertEqual(client.request_cursors, [None, None])
        self.assertEqual(len(client.availability_waits), 1)

    def test_backoff_responses_restart_whole_transaction_without_reusing_cursor(
        self,
    ) -> None:
        retry_cases = (
            (429, "resource_exhausted"),
            (503, "capability_unavailable"),
        )
        for status, code in retry_cases:
            with self.subTest(status=status, code=code):
                expected = ["Phase5 Large Row 04000", "Phase5 Large Row 04001"]
                client = ScriptedApiClient(
                    [
                        (200, element_page(expected[:1], cursor("discarded-cursor"))),
                        (
                            status,
                            retry_problem(status, code, "after_backoff"),
                            {"retry-after": "1"},
                        ),
                        (200, element_page(expected, None)),
                    ]
                )
                with mock.patch.object(MODULE.time, "sleep") as sleep:
                    self.assertEqual(
                        client.collect_name_prefix("Phase5 Large Row 04"),
                        (expected, 1),
                    )
                self.assertEqual(
                    client.request_cursors, [None, cursor("discarded-cursor"), None]
                )
                self.assertEqual(client.availability_waits, [])
                sleep.assert_called_once_with(1.0)

    def test_only_exact_documented_problem_contracts_are_retryable(self) -> None:
        cases = (
            {"status": 409, "code": "permission_denied", "retry": "after_resync"},
            {"status": 409, "code": "stale_reference", "retry": "never"},
            {"status": 409, "code": ["stale_reference"], "retry": "after_resync"},
            {"status": 409, "code": "stale_reference", "retry": ["after_resync"]},
            {"status": 409, "retry": "after_resync"},
            {"status": 409, "code": "stale_reference"},
            {"code": "stale_reference", "retry": "after_resync"},
        )
        for problem in cases:
            with self.subTest(problem=problem):
                client = ScriptedApiClient([(409, problem)])
                with self.assertRaises(MODULE.AcceptanceError):
                    client.collect_name_prefix("Phase5 Large Row 04")
                self.assertEqual(client.request_cursors, [None])
                self.assertEqual(client.availability_waits, [])

    def test_repeated_retryable_conflicts_exhaust_a_bounded_transaction_budget(
        self,
    ) -> None:
        client = ScriptedApiClient(
            [
                (409, stale_problem()),
                (409, stale_problem()),
                (409, stale_problem()),
            ]
        )

        with self.assertRaisesRegex(
            MODULE.AcceptanceError,
            (
                r"retry budget exhausted: status=409 code=stale_reference "
                r"retry=after_resync attempt=3 page=1 cursor_present=false "
                r"instance=urn:xenoteer:request:"
            ),
        ):
            client.collect_name_prefix(
                "Phase5 Large Row 04",
                timeout=1,
                max_transactions=3,
                max_requests=3,
            )
        self.assertEqual(client.request_cursors, [None, None, None])
        self.assertEqual(len(client.availability_waits), 2)

    def test_page_bound_is_terminal(self) -> None:
        client = ScriptedApiClient(
            [
                (200, element_page(["Phase5 one"], cursor("cursor-1"))),
                (200, element_page(["Phase5 two"], cursor("cursor-2"))),
            ]
        )

        with self.assertRaisesRegex(
            MODULE.AcceptanceError,
            "cursor pagination exceeded its fixture bound",
        ):
            client.collect_name_prefix("Phase5", max_pages=2)
        self.assertEqual(client.request_cursors, [None, cursor("cursor-1")])

    def test_malformed_success_pages_fail_closed(self) -> None:
        short_cursor = "a" * 15
        cases = (
            {"elements": None, "next_cursor": None},
            {"elements": [None], "next_cursor": None},
            {"elements": [{"snapshot": {}}], "next_cursor": None},
            {"elements": [], "next_cursor": 7},
            element_page([f"row-{index}" for index in range(26)], None),
            element_page([], ""),
            element_page([], short_cursor),
            element_page([], "cursor with spaces"),
            element_page([], "cursor/with/non-alphabet"),
            element_page([], "a" * (MODULE.MAX_CURSOR_BYTES + 1)),
        )
        for page in cases:
            with self.subTest(page=page):
                client = ScriptedApiClient([(200, page)])
                with self.assertRaises(MODULE.AcceptanceError):
                    client.collect_name_prefix("Phase5")
                self.assertEqual(client.request_cursors, [None])

    def test_protocol_boundaries_are_inclusive(self) -> None:
        first = element_page(["Phase5 A"], "a" * 16)
        reference = first["elements"][0]["snapshot"]["ref"]
        reference["application"]["unique_bus_name"] = ":1." + ("a" * 252)
        reference["application"]["root_object_path"] = "/" + ("a" * 4_095)
        reference["object_path"] = "/" + ("b" * 4_095)
        first["warnings"] = [
            {"code": "w" * 64, "message": "m" * 4_096} for _ in range(32)
        ]
        second = element_page(["Phase5 B"], None)
        client = ScriptedApiClient([(200, first), (200, second)])

        self.assertEqual(
            client.collect_name_prefix("Phase5"),
            (["Phase5 A", "Phase5 B"], 2),
        )
        self.assertEqual(client.request_cursors, [None, "a" * 16])

    def test_terminal_diagnostic_is_bounded_and_redacts_untrusted_problem_data(
        self,
    ) -> None:
        protected = "phase5-secret-never-print"
        token = MODULE.TOKEN_CANARY.decode("ascii")
        client = ScriptedApiClient(
            [
                (
                    409,
                    {
                        "status": 409,
                        "code": protected,
                        "retry": token,
                        "instance": f"urn:xenoteer:request:{protected}:{token}",
                        "detail": f"{protected}:{token}",
                    },
                )
            ]
        )

        with self.assertRaises(MODULE.AcceptanceError) as raised:
            client.collect_name_prefix("Phase5")
        diagnostic = str(raised.exception)
        self.assertLessEqual(len(diagnostic.encode("utf-8")), 512)
        self.assertIn("status=409", diagnostic)
        self.assertIn("code=invalid", diagnostic)
        self.assertIn("retry=invalid", diagnostic)
        self.assertIn("attempt=1", diagnostic)
        self.assertIn("page=1", diagnostic)
        self.assertIn("cursor_present=false", diagnostic)
        self.assertNotIn("instance=", diagnostic)
        self.assertNotIn(protected, diagnostic)
        self.assertNotIn(token, diagnostic)

    def test_failed_resync_wait_reports_only_the_bounded_problem_metadata(
        self,
    ) -> None:
        protected = "phase5-secret-never-print"
        token = MODULE.TOKEN_CANARY.decode("ascii")

        class FailedWaitClient(ScriptedApiClient):
            def wait_accessibility_available(self, *, timeout: float = 60) -> None:
                self.availability_waits.append(timeout)
                raise MODULE.AcceptanceError(f"{protected}:{token}:untrusted-body")

        problem = stale_problem()
        problem["detail"] = f"{protected}:{token}:untrusted-body"
        client = FailedWaitClient([(409, problem)])

        with self.assertRaises(MODULE.AcceptanceError) as raised:
            client.collect_name_prefix("Phase5", timeout=0.25)
        diagnostic = str(raised.exception)
        self.assertLessEqual(len(diagnostic.encode("utf-8")), 512)
        self.assertRegex(
            diagnostic,
            (
                r"retry budget exhausted: status=409 code=stale_reference "
                r"retry=after_resync attempt=1 page=1 cursor_present=false "
                r"instance=urn:xenoteer:request:"
            ),
        )
        self.assertEqual(len(client.availability_waits), 1)
        self.assertGreater(client.availability_waits[0], 0)
        self.assertLessEqual(client.availability_waits[0], 0.25)
        self.assertNotIn(protected, diagnostic)
        self.assertNotIn(token, diagnostic)
        self.assertNotIn("untrusted-body", diagnostic)

    def test_successful_pages_require_one_coherent_transaction(self) -> None:
        first = element_page(["Phase5 A"], cursor("cursor-1"))
        coherent_second = element_page(["Phase5 B"], None)
        cases: list[tuple[str, dict[str, Any]]] = []
        for field, value in (
            ("desktop_id", OTHER_DESKTOP_ID),
            ("desktop_generation", OTHER_DESKTOP_GENERATION),
            ("atspi_generation", "8"),
            ("snapshot_revision", "42"),
            ("order", "preorder"),
            ("truncated", True),
            ("visited_nodes", -1),
        ):
            page = copy.deepcopy(coherent_second)
            page[field] = value
            cases.append((field, page))

        ref_scope = copy.deepcopy(coherent_second)
        ref_scope["elements"][0]["snapshot"]["ref"]["desktop_id"] = OTHER_DESKTOP_ID
        cases.append(("element desktop scope", ref_scope))
        app_scope = copy.deepcopy(coherent_second)
        app_scope["elements"][0]["snapshot"]["ref"]["application"][
            "atspi_generation"
        ] = "8"
        cases.append(("application AT-SPI scope", app_scope))
        future_revision = copy.deepcopy(coherent_second)
        future_revision["elements"][0]["snapshot"]["revision"] = "42"
        cases.append(("element future revision", future_revision))
        duplicate_identity = copy.deepcopy(coherent_second)
        duplicate_identity["elements"][0]["snapshot"]["ref"] = copy.deepcopy(
            first["elements"][0]["snapshot"]["ref"]
        )
        cases.append(("duplicate element identity", duplicate_identity))
        invalid_bus = copy.deepcopy(coherent_second)
        invalid_bus["elements"][0]["snapshot"]["ref"]["application"][
            "unique_bus_name"
        ] = ":1"
        cases.append(("invalid application bus", invalid_bus))
        oversized_bus = copy.deepcopy(coherent_second)
        oversized_bus["elements"][0]["snapshot"]["ref"]["application"][
            "unique_bus_name"
        ] = ":1." + ("a" * 253)
        cases.append(("oversized application bus", oversized_bus))
        invalid_root_path = copy.deepcopy(coherent_second)
        invalid_root_path["elements"][0]["snapshot"]["ref"]["application"][
            "root_object_path"
        ] = "/org//root"
        cases.append(("invalid application root path", invalid_root_path))
        invalid_object_path = copy.deepcopy(coherent_second)
        invalid_object_path["elements"][0]["snapshot"]["ref"][
            "object_path"
        ] = "/org/root/"
        cases.append(("invalid element object path", invalid_object_path))
        oversized_object_path = copy.deepcopy(coherent_second)
        oversized_object_path["elements"][0]["snapshot"]["ref"][
            "object_path"
        ] = "/" + ("a" * 4_096)
        cases.append(("oversized element object path", oversized_object_path))
        missing_ref_key = copy.deepcopy(coherent_second)
        del missing_ref_key["elements"][0]["snapshot"]["ref"]["cache_sequence"]
        cases.append(("missing strict reference field", missing_ref_key))
        missing_app_key = copy.deepcopy(coherent_second)
        del missing_app_key["elements"][0]["snapshot"]["ref"]["application"][
            "identity_hash"
        ]
        cases.append(("missing strict application field", missing_app_key))
        too_many_warnings = copy.deepcopy(coherent_second)
        too_many_warnings["warnings"] = [
            {"code": "test.warning", "message": "bounded"} for _ in range(33)
        ]
        cases.append(("too many warnings", too_many_warnings))
        invalid_warning_code = copy.deepcopy(coherent_second)
        invalid_warning_code["warnings"] = [
            {"code": "Not Safe", "message": "bounded"}
        ]
        cases.append(("invalid warning code", invalid_warning_code))
        invalid_warning_message = copy.deepcopy(coherent_second)
        invalid_warning_message["warnings"] = [
            {"code": "test.warning", "message": "unsafe\0message"}
        ]
        cases.append(("invalid warning message", invalid_warning_message))
        oversized_warning_message = copy.deepcopy(coherent_second)
        oversized_warning_message["warnings"] = [
            {"code": "test.warning", "message": "w" * 4_097}
        ]
        cases.append(("oversized warning message", oversized_warning_message))
        oversized_utf8_warning_message = copy.deepcopy(coherent_second)
        oversized_utf8_warning_message["warnings"] = [
            {"code": "test.warning", "message": "🙂" * 2_000}
        ]
        cases.append(
            ("oversized UTF-8 warning message", oversized_utf8_warning_message)
        )

        for label, second in cases:
            with self.subTest(label=label):
                client = ScriptedApiClient([(200, first), (200, second)])
                with self.assertRaises(MODULE.AcceptanceError):
                    client.collect_name_prefix("Phase5")
                self.assertEqual(
                    client.request_cursors, [None, cursor("cursor-1")]
                )

    def test_successful_pages_reject_cross_page_name_reordering(self) -> None:
        client = ScriptedApiClient(
            [
                (200, element_page(["Phase5 B"], cursor("cursor-1"))),
                (200, element_page(["Phase5 A"], None)),
            ]
        )

        with self.assertRaises(MODULE.AcceptanceError):
            client.collect_name_prefix("Phase5")

    def test_retry_after_header_is_exact_and_never_reported(self) -> None:
        for status, code, headers in (
            (429, "resource_exhausted", {}),
            (429, "resource_exhausted", {"retry-after": "2"}),
            (503, "capability_unavailable", {"retry-after": "secret-value"}),
            (409, "stale_reference", {"retry-after": "1"}),
        ):
            with self.subTest(status=status, headers=headers):
                problem = (
                    stale_problem()
                    if status == 409
                    else retry_problem(status, code, "after_backoff")
                )
                client = ScriptedApiClient([(status, problem, headers)])
                with self.assertRaises(MODULE.AcceptanceError) as raised:
                    client.collect_name_prefix("Phase5")
                diagnostic = str(raised.exception)
                self.assertNotIn("secret-value", diagnostic)
                self.assertNotIn("retry-after", diagnostic.lower())
                self.assertEqual(client.request_cursors, [None])

    def test_absolute_deadline_stops_before_another_request(self) -> None:
        client = ScriptedApiClient(
            [(200, element_page(["Phase5 A"], cursor("cursor-1")))]
        )
        with mock.patch.object(
            MODULE.time, "monotonic", side_effect=(0.0, 0.0, 1.1)
        ):
            with self.assertRaisesRegex(
                MODULE.AcceptanceError, "retry budget exhausted"
            ):
                client.collect_name_prefix("Phase5", timeout=1)
        self.assertEqual(client.request_cursors, [None])

    def test_terminal_success_after_absolute_deadline_is_rejected(self) -> None:
        client = ScriptedApiClient([(200, element_page(["Phase5 A"], None))])
        with mock.patch.object(
            MODULE.time, "monotonic", side_effect=(0.0, 0.0, 1.1)
        ):
            with self.assertRaisesRegex(
                MODULE.AcceptanceError, "retry budget exhausted"
            ):
                client.collect_name_prefix("Phase5", timeout=1)
        self.assertEqual(client.request_cursors, [None])

    def test_request_budget_stops_after_a_successful_continuation_page(self) -> None:
        client = ScriptedApiClient(
            [(200, element_page(["Phase5 A"], cursor("cursor-1")))]
        )
        with mock.patch.object(MODULE.time, "monotonic", return_value=0.0):
            with self.assertRaisesRegex(
                MODULE.AcceptanceError, "retry budget exhausted"
            ):
                client.collect_name_prefix("Phase5", max_requests=1)
        self.assertEqual(client.request_cursors, [None])

    def test_each_request_timeout_is_clipped_to_remaining_deadline(self) -> None:
        client = ScriptedApiClient([(200, element_page(["Phase5 A"], None))])
        with mock.patch.object(
            MODULE.time, "monotonic", side_effect=(10.0, 10.25, 10.5)
        ):
            self.assertEqual(
                client.collect_name_prefix("Phase5", timeout=1),
                (["Phase5 A"], 1),
            )
        self.assertEqual(client.request_timeouts, [0.75])

    def test_backoff_does_not_start_without_a_full_remaining_interval(self) -> None:
        client = ScriptedApiClient(
            [
                (
                    429,
                    retry_problem(429, "resource_exhausted", "after_backoff"),
                    {"retry-after": "1"},
                )
            ]
        )
        with (
            mock.patch.object(
                MODULE.time, "monotonic", side_effect=(0.0, 0.0, 0.75)
            ),
            mock.patch.object(MODULE.time, "sleep") as sleep,
        ):
            with self.assertRaisesRegex(
                MODULE.AcceptanceError, "retry budget exhausted"
            ):
                client.collect_name_prefix("Phase5", timeout=1)
        sleep.assert_not_called()
        self.assertEqual(client.request_cursors, [None])

    def test_successful_pages_preserve_server_order(self) -> None:
        client = ScriptedApiClient(
            [
                (
                    200,
                    element_page(
                        ["Phase5 four", "Phase5 nine"], cursor("cursor-1")
                    ),
                ),
                (200, element_page(["Phase5 two"], None)),
            ]
        )

        self.assertEqual(
            client.collect_name_prefix("Phase5"),
            (["Phase5 four", "Phase5 nine", "Phase5 two"], 2),
        )
        self.assertEqual(client.request_cursors, [None, cursor("cursor-1")])

    def test_large_stress_acceptance_requires_every_exact_ordered_name(self) -> None:
        expected = MODULE.expected_large_stress_names()
        MODULE.require_large_stress_pagination(expected, 4)

        mutations = {
            "changed middle": [
                *expected[:47],
                "Phase5 Large Row WRONG",
                *expected[48:],
            ],
            "swapped middle": [
                *expected[:47],
                expected[48],
                expected[47],
                *expected[49:],
            ],
            "duplicate and omission": [
                *expected[:47],
                expected[46],
                *expected[48:],
            ],
        }
        for label, names in mutations.items():
            with self.subTest(label=label):
                with self.assertRaises(MODULE.AcceptanceError):
                    MODULE.require_large_stress_pagination(names, 4)


if __name__ == "__main__":
    unittest.main()
