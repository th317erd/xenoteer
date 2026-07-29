#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Regression tests for the blocking Phase 6 SDK/package CI contract."""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW_PATH = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
STATIC_GATE_PATH = REPOSITORY_ROOT / "scripts" / "container" / "test-static.sh"
PYTHON_LOCK_PATH = REPOSITORY_ROOT / "packages" / "python" / "requirements-test.lock"
PINNED_ACTION = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")

WORKFLOW_CONTRACT = (
    "CARGO_BUILD_JOBS: 2",
    "RUST_TEST_THREADS: 2",
    "PYTHONDONTWRITEBYTECODE: '1'",
    "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020",
    "node-version: '22'",
    "actions/setup-python@a26af69be951a213d495a4c3e4e4022e16d87065",
    "python-version: '3.11'",
    "cargo test --locked --jobs 2 -p xenoteer-sdk --test conformance_adapter",
    "cargo test --locked --jobs 2 -p xenoteerctl --test cli_boundary",
    "scripts/packages/verify-boundaries.py",
    "npm ci --ignore-scripts",
    "npm test",
    "npm run conformance",
    "npm run verify-pack",
    "for build_dependency in setuptools wheel",
    "--no-deps --requirement packages/python/requirements-test.lock",
    "--no-build-isolation --no-deps --editable packages/python",
    "python3 scripts/conformance/run.py --adapter",
    "packages/python/scripts/run_conformance.py",
    "python3 -m unittest discover -s packages/python/tests",
    "python3 -m build --no-isolation",
    "packages/python/scripts/verify_dist.py",
)

STATIC_PACKAGE_CONTRACT = (
    "packages/typescript/scripts/clean-dist.mjs",
    "packages/typescript/scripts/conformance-adapter.mjs",
    "packages/typescript/scripts/package-allowlist.json",
    "packages/typescript/scripts/verify-package.mjs",
    "packages/python/LICENSE",
    "packages/python/NOTICE",
    "packages/python/MANIFEST.in",
    "packages/python/PACKAGE_ALLOWLIST.txt",
    "packages/python/SDIST_ALLOWLIST.txt",
    "packages/python/WHEEL_ALLOWLIST.txt",
    "packages/python/pyproject.toml",
    "packages/python/requirements-test.lock",
    "packages/python/scripts/run_conformance.py",
    "packages/python/scripts/verify_dist.py",
    "packages/python/src/xenoteer/__init__.py",
)

STATIC_INVENTORY_CONTRACT = (
    "packages/typescript/package.json",
    "packages/typescript/package-lock.json",
    "packages/typescript/scripts/clean-dist.mjs",
    "packages/typescript/scripts/conformance-adapter.mjs",
    "packages/typescript/scripts/package-allowlist.json",
    "packages/typescript/scripts/verify-package.mjs",
    "packages/typescript/src/index.ts",
    "packages/typescript/tsconfig.json",
    "packages/python/MANIFEST.in",
    "packages/python/PACKAGE_ALLOWLIST.txt",
    "packages/python/SDIST_ALLOWLIST.txt",
    "packages/python/WHEEL_ALLOWLIST.txt",
    "packages/python/pyproject.toml",
    "packages/python/requirements-test.lock",
    "packages/python/scripts/run_conformance.py",
    "packages/python/scripts/verify_dist.py",
    "packages/python/src/xenoteer/__init__.py",
)


def validate_action_pins(workflow: str) -> None:
    """Reject every mutable or malformed third-party action reference."""

    action_references = []
    for line in workflow.splitlines():
        match = re.match(r"^\s*-\s+uses:\s+(?P<reference>\S+)(?:\s+#.*)?$", line)
        if match is not None:
            action_references.append(match.group("reference"))
    if not action_references:
        raise AssertionError("workflow has no third-party action references")
    malformed = [
        reference
        for reference in action_references
        if not reference.startswith("./") and PINNED_ACTION.fullmatch(reference) is None
    ]
    if malformed:
        raise AssertionError(f"mutable or malformed action references: {malformed!r}")


def validate_minimal_permissions(workflow: str) -> None:
    """Require the top-level workflow token to expose read-only contents."""

    match = re.search(
        r"(?m)^permissions:\n(?P<body>(?:  [^\n]+\n)+)",
        workflow,
    )
    if match is None:
        raise AssertionError("workflow has no explicit top-level permissions")
    entries = [
        line.strip()
        for line in match.group("body").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if entries != ["contents: read"]:
        raise AssertionError(f"workflow permissions are not minimal: {entries!r}")


def require_fragments(document: str, fragments: tuple[str, ...], label: str) -> None:
    """Require every reviewed contract fragment in one repository document."""

    missing = [fragment for fragment in fragments if fragment not in document]
    if missing:
        raise AssertionError(f"{label} is missing contract fragments: {missing!r}")


def uncommented(document: str) -> str:
    """Discard comment-only lines so prose cannot satisfy an executable contract."""

    return "\n".join(
        line for line in document.splitlines() if not line.lstrip().startswith("#")
    )


def extract_job(workflow: str, name: str) -> str:
    """Extract one top-level job without relying on a YAML package."""

    lines = workflow.splitlines()
    header = f"  {name}:"
    try:
        start = lines.index(header)
    except ValueError as error:
        raise AssertionError(f"CI workflow has no {name!r} job") from error
    body: list[str] = []
    for line in lines[start + 1 :]:
        if re.fullmatch(r"  [A-Za-z0-9_-]+:", line):
            break
        body.append(line)
    if not body:
        raise AssertionError(f"CI workflow job {name!r} is empty")
    return uncommented("\n".join(body))


def required_static_paths(static_gate: str) -> set[str]:
    """Read the literal static-gate required-file array."""

    match = re.search(r"(?ms)^required=\(\n(?P<body>.*?)^\)\n", static_gate)
    if match is None:
        raise AssertionError("container static gate has no required-file array")
    paths = {
        line.strip()
        for line in match.group("body").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    if any(re.search(r"\s", path) for path in paths):
        raise AssertionError("container static required-file array is not literal")
    return paths


def validate_static_contract(static_gate: str) -> None:
    """Require files at admission and again after source inventory generation."""

    missing_required = sorted(
        set(STATIC_PACKAGE_CONTRACT) - required_static_paths(static_gate)
    )
    if missing_required:
        raise AssertionError(
            f"container static required-file array is missing: {missing_required!r}"
        )
    inventory_marker = "scripts/licenses/inventory-first-party.sh . /tmp/xenoteer-first-party.tsv"
    try:
        inventory_checks = static_gate.split(inventory_marker, 1)[1]
    except IndexError as error:
        raise AssertionError("container static gate never generates source inventory") from error
    require_fragments(
        uncommented(inventory_checks),
        STATIC_INVENTORY_CONTRACT,
        "container static inventory checks",
    )


def validate_python_build_lock(lock: str) -> None:
    """Require exact installed build backends before disabling isolation."""

    exact: dict[str, str] = {}
    for line in lock.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        match = re.fullmatch(r"(?P<name>[A-Za-z0-9_.-]+)==(?P<version>[^<>=\s]+)", stripped)
        if match is None:
            raise AssertionError(
                f"Python lock entry is not exactly pinned: {stripped!r}"
            )
        version = match.group("version")
        if any(character in version for character in "*;,@[]()"):
            raise AssertionError(
                f"Python lock entry is not exactly pinned: {stripped!r}"
            )
        name = re.sub(r"[-_.]+", "-", match.group("name").lower())
        if name in exact:
            raise AssertionError(f"Python test lock duplicates {name!r}")
        exact[name] = version
    missing = sorted({"setuptools", "wheel"} - exact.keys())
    if missing:
        raise AssertionError(
            f"Python no-isolation build dependencies are not exactly pinned: {missing!r}"
        )


class PhaseSixCiContractTests(unittest.TestCase):
    """Fail closed when a language or package boundary silently leaves CI."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        cls.static_gate = STATIC_GATE_PATH.read_text(encoding="utf-8")
        cls.python_lock = PYTHON_LOCK_PATH.read_text(encoding="utf-8")

    def test_every_third_party_action_is_immutable(self) -> None:
        validate_action_pins(self.workflow)

    def test_mutable_action_reference_is_rejected(self) -> None:
        with self.assertRaisesRegex(AssertionError, "mutable or malformed"):
            validate_action_pins(
                "steps:\n"
                "  - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262\n"
                "  - uses: actions/setup-node@v4\n"
            )

    def test_local_action_does_not_require_an_external_revision(self) -> None:
        validate_action_pins(
            "steps:\n"
            "  - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262\n"
            "  - uses: ./.github/actions/local-check\n"
        )

    def test_workflow_permissions_are_minimal(self) -> None:
        validate_minimal_permissions(self.workflow)
        with self.assertRaisesRegex(AssertionError, "not minimal"):
            validate_minimal_permissions(
                self.workflow.replace(
                    "permissions:\n  contents: read\n",
                    "permissions:\n  contents: read\n  actions: write\n",
                    1,
                )
            )

    def test_every_sdk_and_package_boundary_is_blocking_in_ci(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-conformance")
        require_fragments(sdk_job, WORKFLOW_CONTRACT, "sdk-conformance CI job")

    def test_missing_language_gate_is_rejected(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-conformance")
        incomplete = sdk_job.replace("npm run conformance", "")
        with self.assertRaisesRegex(AssertionError, "npm run conformance"):
            require_fragments(incomplete, WORKFLOW_CONTRACT, "sdk-conformance CI job")

    def test_comment_cannot_impersonate_a_language_gate(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-conformance")
        incomplete = sdk_job.replace(
            "timeout 3m npm run conformance",
            "# timeout 3m npm run conformance",
        )
        with self.assertRaisesRegex(AssertionError, "npm run conformance"):
            require_fragments(
                uncommented(incomplete),
                WORKFLOW_CONTRACT,
                "sdk-conformance CI job",
            )

    def test_unpinned_python_build_isolation_is_rejected(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-conformance")
        for required in (
            "for build_dependency in setuptools wheel",
            "--no-deps --requirement packages/python/requirements-test.lock",
            "--no-build-isolation --no-deps --editable packages/python",
            "python3 -m build --no-isolation",
        ):
            with self.subTest(required=required):
                incomplete = sdk_job.replace(required, "")
                with self.assertRaisesRegex(AssertionError, re.escape(required)):
                    require_fragments(
                        incomplete,
                        WORKFLOW_CONTRACT,
                        "sdk-conformance CI job",
                    )

    def test_static_gate_tracks_every_reviewed_package_boundary_file(self) -> None:
        validate_static_contract(self.static_gate)

    def test_missing_python_boundary_file_is_rejected(self) -> None:
        incomplete = self.static_gate.replace(
            "  packages/python/WHEEL_ALLOWLIST.txt\n",
            "",
            1,
        )
        with self.assertRaisesRegex(AssertionError, "required-file.*WHEEL_ALLOWLIST"):
            validate_static_contract(incomplete)

    def test_python_no_isolation_backends_are_exactly_locked(self) -> None:
        validate_python_build_lock(self.python_lock)
        for incomplete in (
            re.sub(
                r"(?m)^setuptools==[^\n]+$",
                "setuptools>=0",
                self.python_lock,
            ),
            re.sub(r"(?m)^wheel==[^\n]+\n?", "", self.python_lock),
        ):
            with self.subTest(incomplete=incomplete):
                with self.assertRaisesRegex(AssertionError, "not exactly pinned"):
                    validate_python_build_lock(incomplete)

    def test_every_python_lock_entry_must_be_exact(self) -> None:
        unpinned = re.sub(
            r"(?m)^httpx==[^\n]+$",
            "httpx>=0.27",
            self.python_lock,
        )
        self.assertNotEqual(unpinned, self.python_lock)
        with self.assertRaisesRegex(AssertionError, "not exactly pinned"):
            validate_python_build_lock(unpinned)

        wildcard = re.sub(
            r"(?m)^httpx==[^\n]+$",
            "httpx==0.28.*",
            self.python_lock,
        )
        self.assertNotEqual(wildcard, self.python_lock)
        with self.assertRaisesRegex(AssertionError, "not exactly pinned"):
            validate_python_build_lock(wildcard)

    def test_python_lock_rejects_normalized_duplicate_names(self) -> None:
        duplicate = self.python_lock + "mypy-extensions==1.1.0\n"
        with self.assertRaisesRegex(AssertionError, "duplicates"):
            validate_python_build_lock(duplicate)


if __name__ == "__main__":
    unittest.main()
