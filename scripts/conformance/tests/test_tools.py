# SPDX-License-Identifier: Apache-2.0
"""Failure-path tests for the dependency-free conformance tools."""

from __future__ import annotations

import copy
import json
import pathlib
import shutil
import sys
import tempfile
import unittest


TOOLS_ROOT = pathlib.Path(__file__).resolve().parents[1]
REPOSITORY_ROOT = TOOLS_ROOT.parents[1]
sys.path.insert(0, str(TOOLS_ROOT))

import run  # noqa: E402
import validate  # noqa: E402


class ValidatorTests(unittest.TestCase):
    """Exercise integrity and semantic validation independently of SDK code."""

    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.corpus_root = (
            pathlib.Path(self.temporary_directory.name) / "v1"
        )
        shutil.copytree(REPOSITORY_ROOT / "conformance" / "v1", self.corpus_root)

    def test_checked_in_corpus_loads_with_complete_operation_coverage(self) -> None:
        corpus = validate.load_corpus(REPOSITORY_ROOT / "conformance" / "v1")
        self.assertEqual(len(corpus.suites), 8)
        self.assertEqual(len(corpus.cases), 73)
        self.assertEqual(
            {case["operation"] for case in corpus.cases},
            validate.REQUIRED_OPERATIONS,
        )
        self.assertTrue(
            {
                "command.reconnect.generation-changed",
                "compat.response.invalid-capability-reason",
                "compat.response.invalid-capability-record",
                "compat.response.invalid-desktop-reason",
                "event.cursor-zero",
                "event.duplicate-sequence",
                "event.missing-subscription-request-id",
                "event.queue-full-server-resync",
                "event.sequence-regression",
                "event.stale-subscription-request-id",
                "event.unsubscribed-topic",
                "event.wrong-desktop",
                "event.wrong-generation",
                "event.zero-visible-replay",
            }.issubset({case["id"] for case in corpus.cases})
        )

    def test_byte_tampering_fails_before_case_execution(self) -> None:
        suite = self.corpus_root / "cases" / "uint64-string.json"
        raw = suite.read_bytes()
        suite.write_bytes(raw[:-1] + b" \\n")
        with self.assertRaisesRegex(
            validate.ConformanceError,
            "hash differs",
        ):
            validate.load_corpus(self.corpus_root)

    def test_canonical_uint64_boundaries_are_exact(self) -> None:
        self.assertEqual(
            validate._canonical_uint64("18446744073709551615", True),
            (1 << 64) - 1,
        )
        for value in (
            "18446744073709551616",
            "01",
            "+1",
            "-1",
            "١",
            "",
        ):
            self.assertIsNone(validate._canonical_uint64(value, True))
        self.assertIsNone(validate._canonical_uint64(1, True))
        self.assertIsNone(validate._canonical_uint64("0", False))

    def _suite_case(self, filename: str, index: int = 0) -> dict:
        suite = json.loads(
            (REPOSITORY_ROOT / "conformance" / "v1" / "cases" / filename)
            .read_text(encoding="utf-8")
        )
        return copy.deepcopy(suite["cases"][index])

    def test_redaction_validator_requires_raw_secret_bearing_input(self) -> None:
        case = self._suite_case("redaction.json")
        case["input"]["raw"]["bytes_utf8"] = "already-redacted"
        with self.assertRaisesRegex(
            validate.ConformanceError,
            "raw must contain the secret",
        ):
            validate._validate_case(case, "test", 0)

    def test_redaction_validator_accepts_bearer_prefix_around_raw_secret(self) -> None:
        case = self._suite_case("redaction.json", 1)
        validate._validate_case(case, "test", 0)

    def test_redaction_validator_rejects_wrong_viewer_secret(self) -> None:
        case = self._suite_case("redaction.json", 4)
        case["input"]["secret"] = "different-viewer-secret"
        with self.assertRaisesRegex(
            validate.ConformanceError,
            "raw must contain the secret",
        ):
            validate._validate_case(case, "test", 0)

    def test_scenario_validator_rejects_renamed_fixture_fields(self) -> None:
        case = self._suite_case("event-continuity.json")
        case["input"]["steps"] = case["input"].pop("frames")
        with self.assertRaisesRegex(validate.ConformanceError, "keys differ"):
            validate._validate_case(case, "test", 0)

    def test_scenario_validator_rejects_narrative_assertions(self) -> None:
        case = self._suite_case("effect-stages.json")
        case["expect"] = {"assertions": ["named_claim"]}
        with self.assertRaisesRegex(validate.ConformanceError, "keys differ"):
            validate._validate_case(case, "test", 0)

    def test_event_scenario_rejects_noncanonical_cursors_and_request_id(self) -> None:
        case = self._suite_case("event-continuity.json")
        for field, invalid in (
            ("initial_cursor", "01"),
            ("subscription_request_id", "not-a-uuid"),
        ):
            mutated = copy.deepcopy(case)
            mutated["input"][field] = invalid
            with self.subTest(field=field), self.assertRaisesRegex(
                validate.ConformanceError,
                field,
            ):
                validate._validate_case(mutated, "test", 0)

        mutated = copy.deepcopy(case)
        mutated["expect"]["final_cursor"] = "18446744073709551616"
        with self.assertRaisesRegex(validate.ConformanceError, "final_cursor"):
            validate._validate_case(mutated, "test", 0)

    def test_event_scenario_rejects_unknown_terminal_and_bad_resync_reason(self) -> None:
        case = self._suite_case("event-continuity.json")
        mutated = copy.deepcopy(case)
        mutated["expect"]["terminal"] = "story_says_it_failed"
        with self.assertRaisesRegex(validate.ConformanceError, "terminal"):
            validate._validate_case(mutated, "test", 0)

        mutated = copy.deepcopy(case)
        mutated["expect"]["resync_reason"] = {"narrative": True}
        with self.assertRaisesRegex(validate.ConformanceError, "resync_reason"):
            validate._validate_case(mutated, "test", 0)

    def test_reconnect_envelope_must_match_raw_command(self) -> None:
        case = self._suite_case("command-reconnect.json")
        case["input"]["command"] = {
            "type": "application_launch",
            "application": "mutated",
            "arguments": [],
        }
        with self.assertRaisesRegex(
            validate.ConformanceError,
            "must equal the submitted command",
        ):
            validate._validate_case(case, "test", 0)


class RunnerContractTests(unittest.TestCase):
    """Prove exact result accounting and fail-closed skip behavior."""

    def test_exact_pass_results_are_accepted(self) -> None:
        self.assertEqual(
            run._validate_results(
                {
                    "adapter_protocol": 1,
                    "results": [
                        {"detail": "", "id": "case.one", "status": "passed"}
                    ],
                },
                ("case.one",),
                False,
            ),
            [],
        )

    def test_missing_results_and_skips_fail(self) -> None:
        with self.assertRaisesRegex(
            validate.ConformanceError,
            "result IDs differ",
        ):
            run._validate_results(
                {"adapter_protocol": 1, "results": []},
                ("case.one",),
                False,
            )
        self.assertEqual(
            run._validate_results(
                {
                    "adapter_protocol": 1,
                    "results": [
                        {
                            "detail": "not implemented",
                            "id": "case.one",
                            "status": "skipped",
                        }
                    ],
                },
                ("case.one",),
                False,
            ),
            ["case.one: skipped: not implemented"],
        )

    def test_duplicate_or_malformed_results_are_rejected(self) -> None:
        with self.assertRaisesRegex(
            validate.ConformanceError,
            "malformed",
        ):
            run._validate_results(
                {
                    "adapter_protocol": 1,
                    "results": [
                        {"detail": "", "id": {}, "status": "passed"}
                    ],
                },
                ("case.one",),
                False,
            )


if __name__ == "__main__":
    unittest.main()
