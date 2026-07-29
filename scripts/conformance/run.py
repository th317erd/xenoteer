#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run Xenoteer v1 cases through a language-specific JSON adapter."""

from __future__ import annotations

import argparse
import json
import pathlib
import shlex
import subprocess
import sys
from typing import Any

from validate import ConformanceError, LoadedCorpus, load_corpus


ADAPTER_PROTOCOL = 1
VALID_STATUSES = {"failed", "passed", "skipped"}


def _selected_cases(
    corpus: LoadedCorpus,
    suites: set[str],
    operations: set[str],
) -> tuple[dict[str, Any], ...]:
    selected_suite_names = {
        suite["suite"] for suite in corpus.suites
    } if not suites else suites
    known_suites = {suite["suite"] for suite in corpus.suites}
    unknown_suites = selected_suite_names - known_suites
    if unknown_suites:
        raise ConformanceError(
            f"unknown suite filters: {sorted(unknown_suites)}"
        )
    known_operations = {case["operation"] for case in corpus.cases}
    unknown_operations = operations - known_operations
    if unknown_operations:
        raise ConformanceError(
            f"unknown operation filters: {sorted(unknown_operations)}"
        )
    return tuple(
        case
        for suite in corpus.suites
        if suite["suite"] in selected_suite_names
        for case in suite["cases"]
        if not operations or case["operation"] in operations
    )


def _adapter_payload(
    corpus: LoadedCorpus,
    cases: tuple[dict[str, Any], ...],
) -> dict[str, Any]:
    return {
        "adapter_protocol": ADAPTER_PROTOCOL,
        "cases": cases,
        "corpus": corpus.manifest["corpus"],
        "corpus_sha256": corpus.manifest["corpus_sha256"],
        "protocol": corpus.manifest["protocol"],
    }


def _validate_results(
    value: Any,
    expected_ids: tuple[str, ...],
    allow_skips: bool,
) -> list[str]:
    if not isinstance(value, dict) or set(value) != {
        "adapter_protocol",
        "results",
    }:
        raise ConformanceError("adapter result has an unexpected top-level shape")
    if value["adapter_protocol"] != ADAPTER_PROTOCOL:
        raise ConformanceError("adapter protocol version differs")
    results = value["results"]
    if not isinstance(results, list):
        raise ConformanceError("adapter results must be an array")
    failures: list[str] = []
    seen: set[str] = set()
    for index, result in enumerate(results):
        if not isinstance(result, dict) or set(result) != {
            "detail",
            "id",
            "status",
        }:
            raise ConformanceError(f"adapter result {index} has invalid shape")
        case_id = result["id"]
        status = result["status"]
        detail = result["detail"]
        if (
            not isinstance(case_id, str)
            or not isinstance(status, str)
            or not isinstance(detail, str)
        ):
            raise ConformanceError(f"adapter result {index} is malformed")
        if case_id in seen or status not in VALID_STATUSES:
            raise ConformanceError(f"adapter result {index} is malformed")
        seen.add(case_id)
        if status == "failed" or (status == "skipped" and not allow_skips):
            failures.append(f"{case_id}: {status}: {detail}")
    expected = set(expected_ids)
    if seen != expected:
        raise ConformanceError(
            "adapter result IDs differ; "
            f"missing={sorted(expected - seen)}, extra={sorted(seen - expected)}"
        )
    return failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=pathlib.Path)
    parser.add_argument("--suite", action="append", default=[])
    parser.add_argument("--operation", action="append", default=[])
    parser.add_argument("--timeout-seconds", type=float, default=60.0)
    parser.add_argument("--allow-skips", action="store_true")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--emit-payload", action="store_true")
    parser.add_argument(
        "--adapter",
        nargs=argparse.REMAINDER,
        help="adapter command; receives one JSON document on stdin",
    )
    arguments = parser.parse_args(argv)
    try:
        if arguments.timeout_seconds <= 0:
            raise ConformanceError("--timeout-seconds must be positive")
        corpus = load_corpus(arguments.root)
        cases = _selected_cases(
            corpus,
            set(arguments.suite),
            set(arguments.operation),
        )
        if not cases:
            raise ConformanceError("filters selected no conformance cases")
        if arguments.list:
            for case in cases:
                print(case["id"])
            return 0
        payload = _adapter_payload(corpus, cases)
        encoded = json.dumps(
            payload,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ) + "\n"
        if arguments.emit_payload:
            sys.stdout.write(encoded)
            return 0
        if not arguments.adapter:
            raise ConformanceError(
                "--adapter COMMAND, --list, or --emit-payload is required"
            )
        command = arguments.adapter
        if command and command[0] == "--":
            command = command[1:]
        if not command:
            raise ConformanceError("adapter command is empty")
        try:
            completed = subprocess.run(
                command,
                input=encoded,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=arguments.timeout_seconds,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise ConformanceError(
                f"adapter {shlex.join(command)} did not complete: {error}"
            ) from error
        if completed.returncode != 0:
            raise ConformanceError(
                f"adapter exited {completed.returncode}: "
                f"{completed.stderr.rstrip()}"
            )
        try:
            result = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise ConformanceError(
                f"adapter stdout is not one JSON document: {error}"
            ) from error
        failures = _validate_results(
            result,
            tuple(case["id"] for case in cases),
            arguments.allow_skips,
        )
        if failures:
            print("\n".join(failures), file=sys.stderr)
            return 1
        print(
            f"adapter passed {len(cases)} Xenoteer v1 conformance cases"
        )
        return 0
    except ConformanceError as error:
        print(f"conformance run failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
