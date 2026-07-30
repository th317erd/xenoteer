#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Regression tests for the blocking Phase 6 SDK/package CI contract."""

from __future__ import annotations

import re
import shlex
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW_PATH = REPOSITORY_ROOT / ".github" / "workflows" / "ci.yml"
STATIC_GATE_PATH = REPOSITORY_ROOT / "scripts" / "container" / "test-static.sh"
PYTHON_LOCK_PATH = REPOSITORY_ROOT / "packages" / "python" / "requirements-test.lock"
PINNED_ACTION = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")
SUPPORTED_NODE_VERSIONS = ("22", "24")
SUPPORTED_PYTHON_VERSIONS = ("3.11", "3.12", "3.13", "3.14")
REVIEWED_PYTHON_WHEELS = {
    "anyio==4.14.2": (
        ("anyio-4.14.2-py3-none-any.whl", "9f505dda5ac9f0c8309b5e8bd445a8c2bf7246f3ce950121e45ea15bc41d1494"),
    ),
    "build==1.5.0": (
        ("build-1.5.0-py3-none-any.whl", "13f3eecb844759ab66efec90ca17639bbf14dc06cb2fdf37a9010322d9c50a6f"),
    ),
    "certifi==2026.7.22": (
        ("certifi-2026.7.22-py3-none-any.whl", "62f22742b58a1a33014a2b6b706588a8d7e2a88ae7bd1a6ebe8c992928483775"),
    ),
    "h11==0.16.0": (
        ("h11-0.16.0-py3-none-any.whl", "63cf8bbe7522de3bf65932fda1d9c2772064ffb3dae62d55932da54b31cb6c86"),
    ),
    "httpcore==1.0.9": (
        ("httpcore-1.0.9-py3-none-any.whl", "2d400746a40668fc9dec9810239072b40b4484b640a8c38fd654a024c7a1bf55"),
    ),
    "httpx==0.28.1": (
        ("httpx-0.28.1-py3-none-any.whl", "d909fcccc110f8c7faf814ca82a9a4d816bc5a6dbfea25d6591d6985b8ba59ad"),
    ),
    "idna==3.18": (
        ("idna-3.18-py3-none-any.whl", "7f952cbe720b688055e3f87de14f5c3e5fdaa8bc3928985c4077ca689de849a2"),
    ),
    "librt==0.13.0": (
        ("librt-0.13.0-cp311-cp311-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "7db9a3ff32ef5f7d1703d93831a3316cdf0b537de6a1cc03cc8fdd09b9194e89"),
        ("librt-0.13.0-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "b222493da6e7b6199db9bd79502436cf5a27da3c1f7fa83c7e285444fc93fd03"),
        ("librt-0.13.0-cp313-cp313-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "94b85d664d777bab6c0d709416cb42938251fda9e221b79e3a2215d85df5f4f9"),
        ("librt-0.13.0-cp314-cp314-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "22034924f5b42d5a56371cf271771bfeaabf235a7a8b6264bef2d20013f786c6"),
    ),
    "mypy==1.20.2": (
        ("mypy-1.20.2-cp311-cp311-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "0deb80d062b2479f2c87ae568f89845afc71d11bc41b04179e58165fd9f31e98"),
        ("mypy-1.20.2-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "a5da6976f20cae27059ea8d0c86e7cef3de720e04c4bb9ee18e3690fdb792066"),
        ("mypy-1.20.2-cp313-cp313-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "bb9c2fa06887e21d6a3a868762acb82aec34e2c6fd0174064f27c93ede68ad15"),
        ("mypy-1.20.2-cp314-cp314-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", "5a65aa591af023864fd08a97da9974e919452cfe19cb146c8a5dc692626445dc"),
    ),
    "mypy-extensions==1.1.0": (
        ("mypy_extensions-1.1.0-py3-none-any.whl", "1be4cccdb0f2482337c4743e60421de3a356cd97508abadd57d47403e94f5505"),
    ),
    "packaging==26.2": (
        ("packaging-26.2-py3-none-any.whl", "5fc45236b9446107ff2415ce77c807cee2862cb6fac22b8a73826d0693b0980e"),
    ),
    "pathspec==1.1.1": (
        ("pathspec-1.1.1-py3-none-any.whl", "a00ce642f577bf7f473932318056212bc4f8bfdf53128c78bbd5af0b9b20b189"),
    ),
    "pyproject-hooks==1.2.0": (
        ("pyproject_hooks-1.2.0-py3-none-any.whl", "9e5c6bfa8dcc30091c74b0cf803c81fdd29d94f01992a7707bc97babb1141913"),
    ),
    "ruff==0.15.22": (
        ("ruff-0.15.22-py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl", "365523eb91d9224e1bcb03b022fbf0facb8f9e23792a2c53d9d4b3924bdbdebb"),
    ),
    "setuptools==83.0.0": (
        ("setuptools-83.0.0-py3-none-any.whl", "29b23c360f22f414dc7336bb39178cc7bcbf6021ed2733cde173f09dba19abb3"),
    ),
    "typing-extensions==4.16.0": (
        ("typing_extensions-4.16.0-py3-none-any.whl", "481caa481374e813c1b176ada14e97f1f67a4539ce9cfeb3f350d78d6370c2e8"),
    ),
    "websockets==16.1.1": (
        ("websockets-16.1.1-cp311-cp311-manylinux1_x86_64.manylinux_2_28_x86_64.manylinux_2_5_x86_64.whl", "da4ca1a9d72f9030b3146b8d7022719a9f3d478f61efe6f7dd51d243f61c51b2"),
        ("websockets-16.1.1-cp312-cp312-manylinux1_x86_64.manylinux_2_28_x86_64.manylinux_2_5_x86_64.whl", "0f62863e8a00a6d33c3d6566ec0b89f23787b747ffe0c3bc71ec0e76b82c94b1"),
        ("websockets-16.1.1-cp313-cp313-manylinux1_x86_64.manylinux_2_28_x86_64.manylinux_2_5_x86_64.whl", "f5d497865f05bb222cab7016c6034542e84e5f29f49c6fd3f4939cda7197b5b8"),
        ("websockets-16.1.1-cp314-cp314-manylinux1_x86_64.manylinux_2_28_x86_64.manylinux_2_5_x86_64.whl", "92b820d345f7a3fc7b8163949ee92df910f290c3fc517b3d5301c78065adafe1"),
    ),
    "wheel==0.47.0": (
        ("wheel-0.47.0-py3-none-any.whl", "212281cab4dff978f6cedd499cd893e1f620791ca6ff7107cf270781e587eced"),
    ),
}

RUST_JOB_CONTRACT = (
    "CARGO_BUILD_JOBS: 2",
    "RUST_TEST_THREADS: 2",
)

RUST_WORKFLOW_CONTRACT = (
    "python3 scripts/conformance/validate.py",
    "cargo test --locked --jobs 2 -p xenoteer-sdk --all-targets -- --test-threads=2",
    "cargo test --locked --jobs 2 -p xenoteerctl --all-targets -- --test-threads=2",
    "python3 scripts/packages/verify-boundaries.py",
)

TYPESCRIPT_JOB_CONTRACT = (
    "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020",
    "node-version: ${{ matrix.node }}",
)

TYPESCRIPT_WORKFLOW_CONTRACT = (
    "npm ci --ignore-scripts --no-audit --no-fund",
    "npm test",
    "npm run conformance",
    "npm run verify-pack",
)

PYTHON_JOB_CONTRACT = (
    "PYTHONDONTWRITEBYTECODE: '1'",
    "actions/setup-python@a26af69be951a213d495a4c3e4e4022e16d87065",
    "python-version: ${{ matrix.python }}",
)

PYTHON_WORKFLOW_CONTRACT = (
    "python3 -m pip install --disable-pip-version-check --require-hashes "
    "--only-binary=:all: --no-deps "
    "--requirement packages/python/requirements-test.lock",
    "python3 -m pip install --disable-pip-version-check "
    "--no-build-isolation --no-deps --editable packages/python",
    "python3 -m unittest discover -s packages/python/tests -v",
    "python3 scripts/conformance/run.py --adapter "
    "python3 packages/python/scripts/run_conformance.py",
    'python3 -m mypy --cache-dir "$RUNNER_TEMP/xenoteer-mypy-cache" '
    "packages/python/src packages/python/scripts",
    "python3 -m ruff check --no-cache "
    "packages/python/src packages/python/tests packages/python/scripts",
    'python3 -m build --no-isolation '
    '--outdir "$RUNNER_TEMP/xenoteer-python-dist" packages/python',
    "python3 packages/python/scripts/verify_dist.py "
    '"$RUNNER_TEMP/xenoteer-python-dist/xenoteer-0.1.0-py3-none-any.whl" '
    '"$RUNNER_TEMP/xenoteer-python-dist/xenoteer-0.1.0.tar.gz"',
)

STATIC_PACKAGE_CONTRACT = (
    "scripts/container/qualify-phase6.py",
    "scripts/container/tests/test_phase6_qualification.py",
    "scripts/sdk/qualification_identity.py",
    "scripts/sdk/tests/test_qualification_identity.py",
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
    "scripts/container/qualify-phase6.py",
    "scripts/container/tests/test_phase6_qualification.py",
    "scripts/sdk/qualification_identity.py",
    "scripts/sdk/tests/test_qualification_identity.py",
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

    executable_document = uncommented(document)
    missing = [
        fragment
        for fragment in fragments
        if fragment not in executable_document
    ]
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
    return "\n".join(body)


def extract_executable_commands(job: str) -> tuple[str, ...]:
    """Extract only one-line run scalars whose shell meaning is unambiguous."""

    commands: list[str] = []
    lines = job.splitlines()
    for line_index, line in enumerate(lines):
        match = re.match(r"^(?: {6}- run:| {8}run:)\s*(?P<value>.*)$", line)
        if match is None:
            continue
        for following_line in lines[line_index + 1 :]:
            if not following_line.strip():
                continue
            indentation = len(following_line) - len(
                following_line.lstrip(" ")
            )
            if indentation <= 8:
                break
            raise AssertionError(
                "CI executable run scalar contains continuation content"
            )
        value = match.group("value").strip()
        if value in {"|", "|-", "|+", ">", ">-", ">+"}:
            raise AssertionError(
                "CI executable gate must use one simple run scalar"
            )
        if not value:
            raise AssertionError("CI executable run scalar is empty")
        if (
            "\\" in value
            or any(operator in value for operator in ("&&", "||", "<<", ">>", "$(", "`"))
            or any(character in value for character in (";", "|", "&", "<", ">", "{", "}"))
            or re.search(
                r"(?:^|\s)(?:if|then|elif|else|fi|case|esac|for|while|until|do|done|function)(?:\s|$)",
                value,
            )
            is not None
            or re.search(r"[A-Za-z_][A-Za-z0-9_]*\s*\(\s*\)", value)
            is not None
            or "#" in value
        ):
            raise AssertionError(
                "CI executable run scalar contains unsupported shell control syntax"
            )
        try:
            tokens = shlex.split(value)
        except ValueError as error:
            raise AssertionError("CI executable run scalar is malformed") from error
        if not tokens:
            raise AssertionError("CI executable run scalar is empty")
        commands.append(re.sub(r"\s+", " ", value))
    return tuple(commands)


def require_executable_commands(
    job: str,
    commands: tuple[str, ...],
    label: str,
) -> None:
    """Require each command at an executable position, never in inert YAML/prose."""

    executable = extract_executable_commands(job)
    candidates: list[tuple[str, ...]] = []
    for command in executable:
        try:
            tokens = shlex.split(command)
        except ValueError as error:
            raise AssertionError(f"{label} has malformed executable shell text") from error
        if not tokens:
            continue
        if tokens[0] == "timeout":
            if len(tokens) < 3 or re.fullmatch(r"[1-9][0-9]*(?:s|m|h)", tokens[1]) is None:
                raise AssertionError(
                    f"{label} has a malformed timeout command"
                )
            tokens = tokens[2:]
        candidates.append(tuple(tokens))
    missing: list[str] = []
    for required in commands:
        try:
            required_tokens = tuple(shlex.split(required))
        except ValueError as error:
            raise AssertionError(
                f"{label} has malformed required shell text"
            ) from error
        if required_tokens not in candidates:
            missing.append(required)
    if missing:
        raise AssertionError(
            f"{label} is missing executable commands: {missing!r}"
        )


def require_runtime_matrix(
    job: str,
    axis: str,
    expected: tuple[str, ...],
) -> None:
    """Require an explicit, ordered runtime matrix with no implicit aliases."""

    job = uncommented(job)
    match = re.search(
        rf"(?m)^\s+{re.escape(axis)}:\s*\[(?P<versions>[^\]]+)\]\s*$",
        job,
    )
    if match is None:
        raise AssertionError(f"CI job has no explicit {axis!r} runtime matrix")
    versions = tuple(
        value.strip().strip("'\"")
        for value in match.group("versions").split(",")
        if value.strip()
    )
    if versions != expected:
        raise AssertionError(
            f"{axis} runtime matrix must be exactly {expected!r}, observed {versions!r}"
        )
    interpolation = f"${{{{ matrix.{axis} }}}}"
    if interpolation not in job:
        raise AssertionError(
            f"{axis} runtime matrix is not connected to its setup action"
        )


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


def validate_container_python_test_partition(static_gate: str) -> None:
    """Require all container Python modules under explicit bounded commands."""

    static_gate = uncommented(static_gate)
    package_boundary_command = (
        "timeout 90s env PYTHONDONTWRITEBYTECODE=1 python3 \\\n"
        "  scripts/packages/verify-boundaries.py"
    )
    if package_boundary_command not in static_gate:
        raise AssertionError(package_boundary_command)
    local_image_command = (
        "timeout 90s env PYTHONDONTWRITEBYTECODE=1 python3 \\\n"
        "  scripts/container/tests/test_local_image_build_references.py"
    )
    if local_image_command not in static_gate:
        raise AssertionError(local_image_command)
    bounded_loop = (
        "for bounded_container_python_test in \\\n"
        "  scripts/container/tests/test_host_rust_toolchain.py \\\n"
        "  scripts/container/tests/test_phase5_atspi_live.py \\\n"
        "  scripts/container/tests/test_phase6_qualification.py; do"
    )
    if bounded_loop not in static_gate:
        raise AssertionError(bounded_loop)
    bounded_command = (
        "timeout 10s env PYTHONDONTWRITEBYTECODE=1 \\\n"
        '    python3 "$bounded_container_python_test"'
    )
    if bounded_command not in static_gate:
        raise AssertionError(bounded_command)
    broad_discovery = (
        "python3 -m unittest discover \\\n  -s scripts/container/tests"
    )
    if broad_discovery in static_gate:
        raise AssertionError(broad_discovery)


def validate_python_wheel_artifacts(
    artifacts: dict[str, tuple[tuple[str, str], ...]],
) -> None:
    """Prove every reviewed artifact is a wheel covering each claimed runtime."""

    expected_runtime_tags = {"cp311", "cp312", "cp313", "cp314"}
    for pin, wheels in artifacts.items():
        if not wheels:
            raise AssertionError(f"Python pin has no reviewed wheels: {pin!r}")
        covered: set[str] = set()
        for filename, digest in wheels:
            if not filename.endswith(".whl"):
                raise AssertionError(
                    f"reviewed Python artifact is not a wheel: {filename!r}"
                )
            if re.fullmatch(r"[0-9a-f]{64}", digest) is None:
                raise AssertionError(
                    f"reviewed Python wheel has malformed SHA-256: {filename!r}"
                )
            if "-py3-none-" in filename:
                covered.update(expected_runtime_tags)
            else:
                match = re.search(r"-(cp3(?:11|12|13|14))-", filename)
                if match is None:
                    raise AssertionError(
                        f"reviewed Python wheel has unsupported tags: {filename!r}"
                    )
                covered.add(match.group(1))
            if "x86_64" not in filename and "-none-any.whl" not in filename:
                raise AssertionError(
                    f"reviewed Python wheel is not Linux x86_64: {filename!r}"
                )
        if covered != expected_runtime_tags:
            raise AssertionError(
                f"reviewed Python wheels do not cover every runtime for {pin!r}: "
                f"{sorted(covered)!r}"
            )


def validate_python_build_lock(lock: str) -> None:
    """Require exact pins and reviewed wheel hashes without parser ambiguity."""

    validate_python_wheel_artifacts(REVIEWED_PYTHON_WHEELS)
    exact: dict[str, tuple[str, frozenset[str]]] = {}
    for line in lock.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if "\\" in stripped or "#" in stripped:
            raise AssertionError(
                f"Python lock entry uses an unsafe continuation or inline comment: {stripped!r}"
            )
        match = re.fullmatch(
            r"(?P<name>[A-Za-z0-9_.-]+)==(?P<version>[^<>=\s]+)"
            r"(?P<hashes>(?: --hash=sha256:[0-9a-f]{64})+)",
            stripped,
        )
        if match is None:
            raise AssertionError(
                f"Python lock entry is not exactly pinned with SHA-256 hashes: {stripped!r}"
            )
        version = match.group("version")
        if any(character in version for character in "*;,@[]()"):
            raise AssertionError(
                f"Python lock entry is not exactly pinned: {stripped!r}"
            )
        name = re.sub(r"[-_.]+", "-", match.group("name").lower())
        if name in exact:
            raise AssertionError(f"Python test lock duplicates {name!r}")
        hashes = frozenset(
            token.removeprefix("--hash=sha256:")
            for token in match.group("hashes").split()
        )
        if len(hashes) != len(match.group("hashes").split()):
            raise AssertionError(f"Python test lock duplicates a hash for {name!r}")
        exact[name] = (version, hashes)
    missing = sorted({"setuptools", "wheel"} - exact.keys())
    if missing:
        raise AssertionError(
            f"Python no-isolation build dependencies are not exactly pinned: {missing!r}"
        )
    observed_pins = {f"{name}=={version}" for name, (version, _hashes) in exact.items()}
    expected_pins = set(REVIEWED_PYTHON_WHEELS)
    if observed_pins != expected_pins:
        raise AssertionError(
            "Python lock does not match the reviewed wheel pin set: "
            f"missing={sorted(expected_pins - observed_pins)!r}, "
            f"extra={sorted(observed_pins - expected_pins)!r}"
        )
    for name, (version, hashes) in exact.items():
        pin = f"{name}=={version}"
        expected_hashes = frozenset(
            digest for _filename, digest in REVIEWED_PYTHON_WHEELS[pin]
        )
        if hashes != expected_hashes:
            raise AssertionError(
                f"Python lock hashes differ from reviewed wheels for {pin!r}"
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
        rust_job = extract_job(self.workflow, "sdk-rust")
        require_fragments(
            rust_job,
            RUST_JOB_CONTRACT,
            "Rust SDK CI job",
        )
        require_executable_commands(
            rust_job, RUST_WORKFLOW_CONTRACT, "Rust SDK CI job"
        )
        typescript_job = extract_job(self.workflow, "sdk-typescript")
        require_fragments(
            typescript_job,
            TYPESCRIPT_JOB_CONTRACT,
            "TypeScript SDK CI job",
        )
        require_executable_commands(
            typescript_job,
            TYPESCRIPT_WORKFLOW_CONTRACT,
            "TypeScript SDK CI job",
        )
        python_job = extract_job(self.workflow, "sdk-python")
        require_fragments(
            python_job,
            PYTHON_JOB_CONTRACT,
            "Python SDK CI job",
        )
        require_executable_commands(
            python_job, PYTHON_WORKFLOW_CONTRACT, "Python SDK CI job"
        )

    def test_current_lts_and_claimed_python_runtimes_are_exercised(self) -> None:
        typescript_job = extract_job(self.workflow, "sdk-typescript")
        require_runtime_matrix(
            typescript_job,
            "node",
            SUPPORTED_NODE_VERSIONS,
        )
        python_job = extract_job(self.workflow, "sdk-python")
        require_runtime_matrix(
            python_job,
            "python",
            SUPPORTED_PYTHON_VERSIONS,
        )

    def test_missing_runtime_matrix_member_is_rejected(self) -> None:
        with self.assertRaisesRegex(AssertionError, "runtime matrix must be exactly"):
            require_runtime_matrix(
                "strategy:\n"
                "  matrix:\n"
                "    node: ['22']\n"
                "node-version: ${{ matrix.node }}\n",
                "node",
                SUPPORTED_NODE_VERSIONS,
            )

    def test_rust_package_and_conformance_work_is_not_matrix_multiplied(self) -> None:
        rust_job = extract_job(self.workflow, "sdk-rust")
        self.assertNotIn("strategy:", rust_job)
        self.assertEqual(
            self.workflow.count("scripts/packages/verify-boundaries.py"),
            1,
        )
        self.assertEqual(
            self.workflow.count(
                "cargo test --locked --jobs 2 -p xenoteer-sdk "
                "--all-targets"
            ),
            1,
        )

    def test_missing_language_gate_is_rejected(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-typescript")
        incomplete = sdk_job.replace(
            "        run: timeout 3m npm run conformance\n",
            "",
        )
        with self.assertRaisesRegex(AssertionError, "executable.*npm run conformance"):
            require_executable_commands(
                incomplete,
                TYPESCRIPT_WORKFLOW_CONTRACT,
                "TypeScript SDK CI job",
            )

    def test_comment_cannot_impersonate_a_language_gate(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-typescript")
        incomplete = sdk_job.replace(
            "        run: timeout 3m npm run conformance",
            "        # run: timeout 3m npm run conformance",
        )
        with self.assertRaisesRegex(AssertionError, "executable.*npm run conformance"):
            require_executable_commands(
                incomplete,
                TYPESCRIPT_WORKFLOW_CONTRACT,
                "TypeScript SDK CI job",
            )

    def test_unpinned_python_build_isolation_is_rejected(self) -> None:
        sdk_job = extract_job(self.workflow, "sdk-python")
        for required, mutation_target in (
            (
                "python3 -m pip install --disable-pip-version-check "
                "--require-hashes --only-binary=:all: --no-deps "
                "--requirement packages/python/requirements-test.lock",
                "--require-hashes",
            ),
            (
                "python3 -m pip install --disable-pip-version-check "
                "--no-build-isolation --no-deps --editable packages/python",
                "--editable packages/python",
            ),
            ("python3 -m build --no-isolation", "--no-isolation"),
        ):
            with self.subTest(required=required):
                incomplete = sdk_job.replace(mutation_target, "", 1)
                self.assertNotEqual(incomplete, sdk_job)
                with self.assertRaisesRegex(AssertionError, re.escape(required)):
                    require_executable_commands(
                        incomplete,
                        PYTHON_WORKFLOW_CONTRACT,
                        "Python SDK CI job",
                    )

    def test_static_gate_tracks_every_reviewed_package_boundary_file(self) -> None:
        validate_static_contract(self.static_gate)

    def test_canonical_phase6_runner_is_static_but_not_an_ordinary_ci_live_gate(
        self,
    ) -> None:
        validate_static_contract(self.static_gate)
        validate_container_python_test_partition(self.static_gate)
        self.assertNotIn("scripts/container/qualify-phase6.py", self.workflow)

    def test_container_python_partition_mutations_are_rejected(self) -> None:
        timeout_mutations = (
            self.static_gate.replace(
                "timeout 90s env PYTHONDONTWRITEBYTECODE=1 python3 \\\n"
                "  scripts/packages/verify-boundaries.py",
                "timeout 91s env PYTHONDONTWRITEBYTECODE=1 python3 \\\n"
                "  scripts/packages/verify-boundaries.py",
                1,
            ),
            self.static_gate.replace(
                "timeout 90s env PYTHONDONTWRITEBYTECODE=1 python3 \\\n"
                "  scripts/container/tests/test_local_image_build_references.py",
                "timeout 91s env PYTHONDONTWRITEBYTECODE=1 python3 \\\n"
                "  scripts/container/tests/test_local_image_build_references.py",
                1,
            ),
            self.static_gate.replace(
                "timeout 10s env PYTHONDONTWRITEBYTECODE=1 \\\n"
                '    python3 "$bounded_container_python_test"',
                "timeout 11s env PYTHONDONTWRITEBYTECODE=1 \\\n"
                '    python3 "$bounded_container_python_test"',
                1,
            ),
        )
        for incomplete in timeout_mutations:
            with self.subTest(mutation="timeout"):
                self.assertNotEqual(incomplete, self.static_gate)
                with self.assertRaises(AssertionError):
                    validate_container_python_test_partition(incomplete)

        marker = "for bounded_container_python_test in \\\n"
        partition_index = self.static_gate.index(marker)
        for path in (
            "scripts/container/tests/test_host_rust_toolchain.py",
            "scripts/container/tests/test_phase5_atspi_live.py",
            "scripts/container/tests/test_phase6_qualification.py",
        ):
            with self.subTest(path=path):
                prefix = self.static_gate[:partition_index]
                partition = self.static_gate[partition_index:]
                incomplete = prefix + partition.replace(path, "", 1)
                with self.assertRaisesRegex(AssertionError, re.escape(path)):
                    validate_container_python_test_partition(incomplete)

    def test_missing_phase6_runner_contract_is_rejected(self) -> None:
        for path in (
            "scripts/container/qualify-phase6.py",
            "scripts/container/tests/test_phase6_qualification.py",
            "scripts/sdk/qualification_identity.py",
            "scripts/sdk/tests/test_qualification_identity.py",
        ):
            with self.subTest(path=path):
                incomplete = self.static_gate.replace(f"  {path}\n", "", 1)
                self.assertNotEqual(incomplete, self.static_gate)
                with self.assertRaisesRegex(
                    AssertionError,
                    re.escape(path),
                ):
                    validate_static_contract(incomplete)

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
        entry = next(
            line
            for line in self.python_lock.splitlines()
            if line.startswith("mypy_extensions==")
        )
        duplicate = self.python_lock + entry + "\n"
        with self.assertRaisesRegex(AssertionError, "duplicates"):
            validate_python_build_lock(duplicate)

    def test_python_lock_is_cryptographically_hashed(self) -> None:
        unhashed = [
            line
            for line in self.python_lock.splitlines()
            if line and not line.startswith("#") and "--hash=sha256:" not in line
        ]
        self.assertEqual(unhashed, [])

    def test_python_ci_enforces_hashes_and_binary_wheels(self) -> None:
        python_job = extract_job(self.workflow, "sdk-python")
        require_executable_commands(
            python_job,
            (
                "python3 -m pip install --disable-pip-version-check "
                "--require-hashes --only-binary=:all: --no-deps "
                "--requirement packages/python/requirements-test.lock",
            ),
            "Python hashed install",
        )
        self.assertNotIn("for build_dependency in setuptools wheel", python_job)

    def test_required_command_must_be_in_an_executable_run_scalar(self) -> None:
        inert_jobs = (
            "    steps:\n      - name: npm test\n        run: true\n",
            "    steps:\n      - env:\n          CLAIMED: npm test\n        run: true\n",
            "    steps:\n      - run: |\n          # npm test\n          true\n",
            "    steps:\n      - run: \"echo 'npm test'\"\n",
        )
        for inert_job in inert_jobs:
            with self.subTest(inert_job=inert_job):
                with self.assertRaisesRegex(AssertionError, "executable"):
                    require_executable_commands(
                        inert_job,
                        ("npm test",),
                        "synthetic CI job",
                    )

    def test_required_command_rejects_inert_shell_structures(self) -> None:
        inert_jobs = (
            (
                "heredoc",
                "    steps:\n"
                "      - run: |\n"
                "          cat <<'GATE'\n"
                "          npm test\n"
                "          GATE\n",
            ),
            (
                "uncalled-function",
                "    steps:\n"
                "      - run: |\n"
                "          required_gate() {\n"
                "            npm test\n"
                "          }\n",
            ),
            (
                "false-conditional",
                "    steps:\n"
                "      - run: |\n"
                "          if false; then\n"
                "            npm test\n"
                "          fi\n",
            ),
        )
        for label, inert_job in inert_jobs:
            with self.subTest(label=label):
                with self.assertRaisesRegex(
                    AssertionError,
                    "executable|simple|control",
                ):
                    require_executable_commands(
                        inert_job,
                        ("npm test",),
                        "synthetic CI job",
                    )

    def test_required_command_rejects_yaml_scalar_continuations(self) -> None:
        continued_jobs = (
            (
                "or-fallback",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "          || true\n",
            ),
            (
                "background",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "          &\n",
            ),
            (
                "pipe",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "          | cat\n",
            ),
            (
                "split-suffix",
                "    steps:\n"
                "      - run: timeout 9m npm\n"
                "          test\n",
            ),
            (
                "split-prefix",
                "    steps:\n"
                "      - run: timeout 9m\n"
                "          npm test\n",
            ),
            (
                "quoted",
                "    steps:\n"
                "      - run: \"timeout 9m npm test\n"
                "          || true\"\n",
            ),
            (
                "comment",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "          # a continuation must never be ignored\n",
            ),
            (
                "benign-looking",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "          extra-argument\n",
            ),
            (
                "minimum-deeper-indent",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "         || true\n",
            ),
            (
                "after-blank-line",
                "    steps:\n"
                "      - run: timeout 9m npm test\n"
                "\n"
                "          || true\n",
            ),
        )
        for label, continued_job in continued_jobs:
            with self.subTest(label=label):
                with self.assertRaisesRegex(AssertionError, "continuation"):
                    require_executable_commands(
                        continued_job,
                        ("npm test",),
                        "synthetic CI job",
                    )

    def test_next_yaml_key_and_step_are_not_run_continuations(self) -> None:
        job = (
            "    steps:\n"
            "      - run: timeout 9m npm test\n"
            "        env:\n"
            "          XENOTEER_MODE: strict\n"
            "      - name: A separate step\n"
            "        run: true\n"
        )
        require_executable_commands(job, ("npm test",), "synthetic CI job")

    def test_extracted_job_preserves_run_continuation_comments(self) -> None:
        workflow = (
            "jobs:\n"
            "  synthetic:\n"
            "    steps:\n"
            "      - run: timeout 9m npm test\n"
            "          # physical continuation content\n"
            "  following:\n"
            "    steps:\n"
            "      - run: true\n"
        )
        job = extract_job(workflow, "synthetic")
        with self.assertRaisesRegex(AssertionError, "continuation"):
            require_executable_commands(job, ("npm test",), "synthetic CI job")

    def test_preserved_job_comments_cannot_impersonate_metadata(self) -> None:
        with self.assertRaisesRegex(AssertionError, "contract fragments"):
            require_fragments(
                "# CARGO_BUILD_JOBS: 2\n",
                ("CARGO_BUILD_JOBS: 2",),
                "synthetic CI job",
            )
        with self.assertRaisesRegex(AssertionError, "runtime matrix"):
            require_runtime_matrix(
                "# node: ['22', '24']\n",
                "node",
                SUPPORTED_NODE_VERSIONS,
            )

    def test_every_required_command_rejects_shell_masking_and_prefix_impostors(
        self,
    ) -> None:
        for required in (
            *RUST_WORKFLOW_CONTRACT,
            *TYPESCRIPT_WORKFLOW_CONTRACT,
            *PYTHON_WORKFLOW_CONTRACT,
        ):
            valid = (
                "    steps:\n"
                f"      - run: timeout 9m {required}\n"
            )
            require_executable_commands(valid, (required,), "synthetic CI job")
            mutations = (
                f"{required}-impostor",
                f"{required} || true",
                f"{required} && true",
                f"{required} &",
                f"{required} ; true",
                f"{required} | cat",
                f"{required} # hidden bypass",
                f"({required})",
                f"$({required})",
                f"eval {shlex.quote(required)}",
                f"sh -c {shlex.quote(required)}",
                f"echo {shlex.quote(required)}",
            )
            for mutation in mutations:
                with self.subTest(required=required, mutation=mutation):
                    invalid = (
                        "    steps:\n"
                        f"      - run: timeout 9m {mutation}\n"
                    )
                    with self.assertRaisesRegex(AssertionError, "executable"):
                        require_executable_commands(
                            invalid,
                            (required,),
                            "synthetic CI job",
                        )

    def test_python_lock_rejects_hash_and_parser_bypasses(self) -> None:
        first_hash = re.search(r"(?<=--hash=sha256:)[0-9a-f]{64}", self.python_lock)
        assert first_hash is not None
        malformed = (
            self.python_lock[: first_hash.start()]
            + "not-a-sha256"
            + self.python_lock[first_hash.end() :]
        )
        altered = (
            self.python_lock[: first_hash.start()]
            + ("0" * 64)
            + self.python_lock[first_hash.end() :]
        )
        unhashed = re.sub(
            r"(?m)^(anyio==[^ ]+).*$",
            r"\1",
            self.python_lock,
            count=1,
        )
        continuation = self.python_lock.replace(
            " --hash=sha256:", " \\\n  --hash=sha256:", 1
        )
        commented = re.sub(
            r"(?m)^(anyio==[^ ]+).*$",
            r"\1 # --hash=sha256:" + ("0" * 64),
            self.python_lock,
            count=1,
        )
        for invalid in (malformed, altered, unhashed, continuation, commented):
            with self.subTest(invalid=invalid):
                with self.assertRaises(AssertionError):
                    validate_python_build_lock(invalid)

    def test_reviewed_python_artifacts_reject_source_distributions(self) -> None:
        artifacts = dict(REVIEWED_PYTHON_WHEELS)
        filename, digest = artifacts["anyio==4.14.2"][0]
        artifacts["anyio==4.14.2"] = (
            (filename.removesuffix(".whl") + ".tar.gz", digest),
        )
        with self.assertRaisesRegex(AssertionError, "not a wheel"):
            validate_python_wheel_artifacts(artifacts)

    def test_rust_sdk_and_cli_are_excluded_then_tested_once_completely(self) -> None:
        rust_job = extract_job(self.workflow, "rust")
        rust_commands = extract_executable_commands(rust_job)
        self.assertIn(
            "cargo test --workspace --all-targets --locked "
            "--exclude xenoteer-sdk --exclude xenoteerctl",
            rust_commands,
        )
        sdk_job = extract_job(self.workflow, "sdk-rust")
        sdk_commands = extract_executable_commands(sdk_job)
        self.assertEqual(
            sum(
                "cargo test --locked --jobs 2 -p xenoteer-sdk --all-targets"
                in command
                for command in sdk_commands
            ),
            1,
        )
        self.assertEqual(
            sum(
                "cargo test --locked --jobs 2 -p xenoteerctl --all-targets"
                in command
                for command in sdk_commands
            ),
            1,
        )


if __name__ == "__main__":
    unittest.main()
