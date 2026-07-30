#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Fail-closed unit contracts for the staged public quick-start gate."""

from __future__ import annotations

import copy
import importlib.util
import io
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
MODULE_PATH = REPOSITORY_ROOT / "scripts" / "sdk" / "public_quickstarts.py"
SPEC = importlib.util.spec_from_file_location("public_quickstarts", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load public quick-start gate module")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

EXAMPLE_PATHS = {
    "rust": (
        REPOSITORY_ROOT
        / "crates"
        / "xenoteer-sdk"
        / "examples"
        / "phase6_behaviors.rs"
    ),
    "typescript": (
        REPOSITORY_ROOT
        / "packages"
        / "typescript"
        / "examples"
        / "phase6-behaviors.mjs"
    ),
    "python": (
        REPOSITORY_ROOT
        / "packages"
        / "python"
        / "src"
        / "xenoteer"
        / "examples"
        / "phase6_behaviors.py"
    ),
}
TRANSPORT_DEADLINE = "TRANSPORT_REQUEST_TIMEOUT_MILLISECONDS"
SERVER_LONG_POLL_DEADLINE = "SERVER_LONG_POLL_TIMEOUT_MILLISECONDS"
EXAMPLE_OVERALL_DEADLINE = "EXAMPLE_OVERALL_TIMEOUT_MILLISECONDS"
EXTERNAL_PROCESS_DEADLINE = "EXTERNAL_PROCESS_TIMEOUT_MILLISECONDS"


def _require_contract(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def _require_shape(
    source: str,
    expected: int,
    patterns: tuple[str, ...],
    message: str,
) -> None:
    counts = [len(re.findall(pattern, source)) for pattern in patterns]
    _require_contract(
        all(count == expected for count in counts),
        f"{message}: expected {expected}, observed {counts}",
    )


def _integer_constant(source: str, name: str, language: str) -> int:
    type_annotation = r"(?:\s*:\s*u(?:32|64))?" if language == "rust" else ""
    matches = re.findall(
        rf"\b{re.escape(name)}{type_annotation}\s*=\s*([0-9_]+)",
        source,
    )
    _require_contract(len(matches) == 1, f"{language} must define {name} exactly once")
    return int(matches[0].replace("_", ""))


def validate_package_example_deadlines() -> None:
    """Check the deliberately small, reviewed Phase 6 example manifest.

    Exact shape counts are intentional: a new constructor or wait fails closed
    until this contract is updated with its reviewed deadline expression.
    Deliberate computed/dynamic property obfuscation is outside this source-shape
    contract; the exact staged-package E2E remains the behavioral backstop.
    """
    sources = {
        language: path.read_text(encoding="utf-8")
        for language, path in EXAMPLE_PATHS.items()
    }
    transport_deadlines: dict[str, int] = {}
    server_deadlines: dict[str, int] = {}
    for language, source in sources.items():
        transport_deadlines[language] = _integer_constant(
            source,
            TRANSPORT_DEADLINE,
            language,
        )
        server_deadlines[language] = _integer_constant(
            source,
            SERVER_LONG_POLL_DEADLINE,
            language,
        )
        _require_contract(
            transport_deadlines[language] == 35_000,
            f"{language} transport deadline must remain 35 seconds",
        )
        _require_contract(
            0 < server_deadlines[language] <= 30_000,
            f"{language} server long-poll deadline must remain bounded",
        )

    shapes = {
        "rust": (
            (2, (r"\bClient\b",), "Rust Client symbol references changed"),
            (
                6,
                (r"\bXenoteerClient\b",),
                "Rust XenoteerClient symbol references changed",
            ),
            (
                1,
                (
                    r"\bClient::new\b",
                    r"\bXenoteerClient::from_transport\b",
                    r"with_request_timeout\(\s*Duration::from_millis\(\s*"
                    rf"{TRANSPORT_DEADLINE}\s*,?\s*\)\s*\)",
                ),
                "every Rust constructor must remain in its bounded helper",
            ),
            (
                3,
                (
                    r"\.(?:windows|accessibility)\(\)\s*\.wait\b",
                    r"\.wait\s*\(",
                    r"\b(?:Window|Element)WaitRequest\s*\{",
                    rf"\btimeout_ms\s*:\s*{SERVER_LONG_POLL_DEADLINE}\s*,",
                ),
                "every Rust wait must use the reviewed direct deadline",
            ),
        ),
        "typescript": (
            (
                3,
                (r"\bXenoteerClient\b",),
                "TypeScript XenoteerClient symbol references changed",
            ),
            (
                2,
                (
                    r"\bXenoteerClient\.connect\b",
                    r"\.connect\s*\(",
                    rf"\brequestTimeoutMs\s*:\s*{TRANSPORT_DEADLINE}\s*,",
                ),
                "every TypeScript connect must use the named deadline",
            ),
            (
                3,
                (
                    r"\.(?:windows|accessibility)\.wait\b",
                    r"\.wait\s*\(",
                    r"\btimeout_ms\s*:\s*"
                    rf"(?:{SERVER_LONG_POLL_DEADLINE}|15_000)\s*,",
                ),
                "every TypeScript wait must use a reviewed direct deadline",
            ),
        ),
        "python": (
            (
                4,
                (r"\bXenoteerClient\b",),
                "Python XenoteerClient symbol references changed",
            ),
            (
                3,
                (r"\bClientOptions\b",),
                "Python ClientOptions symbol references changed",
            ),
            (
                2,
                (
                    r"\bXenoteerClient\.connect\b",
                    r"\.connect\s*\(",
                    r"\bClientOptions\s*\(",
                    rf"\brequest_timeout\s*=\s*{TRANSPORT_DEADLINE}\s*/\s*1_000\s*,",
                ),
                "every Python connect/options path must use the named deadline",
            ),
            (
                3,
                (
                    r"\.(?:windows|accessibility)\.wait\b",
                    r"\.wait\s*\(",
                    r"""["']timeout_ms["']\s*:\s*"""
                    rf"(?:{SERVER_LONG_POLL_DEADLINE}|15_000)\s*,",
                ),
                "every Python wait must use a reviewed direct deadline",
            ),
        ),
    }
    for language, language_shapes in shapes.items():
        for expected, patterns, message in language_shapes:
            _require_shape(sources[language], expected, patterns, message)

    python = sources["python"]
    _require_contract(
        len(re.findall(r"\boptions\s*=\s*ClientOptions\s*\(", python))
        == len(re.findall(r"\bXenoteerClient\.connect\s*\(\s*options\s*\)", python))
        == 1,
        "Python main connect must consume its reviewed options data flow",
    )

    external_deadline = MODULE.QUICKSTART_COMMAND_TIMEOUT_SECONDS * 1_000
    _require_contract(
        external_deadline == 120_000
        and MODULE.PACKAGE_COMMAND_TIMEOUT_SECONDS * 1_000 == external_deadline,
        "public package process deadlines must remain exactly 120 seconds",
    )
    for language in ("rust", "python"):
        internal_deadline = _integer_constant(
            sources[language],
            EXAMPLE_OVERALL_DEADLINE,
            language,
        )
        _require_contract(
            internal_deadline == 110_000,
            f"{language} internal deadline must remain exactly 110 seconds",
        )
        _require_contract(
            internal_deadline >= 2 * transport_deadlines[language]
            and internal_deadline < external_deadline,
            f"{language} internal deadline no longer covers work plus cleanup",
        )
    rust = sources["rust"]
    _require_contract(
        len(
            re.findall(
                r"tokio::time::timeout\(\s*Duration::from_millis\(\s*"
                rf"{EXAMPLE_OVERALL_DEADLINE}\s*,?\s*\)\s*,\s*exercise\(\)",
                rust,
            )
        )
        == 1,
        "Rust runtime must use the named internal deadline",
    )
    _require_contract(
        len(
            re.findall(
                r"asyncio\.wait_for\(\s*exercise\(\)\s*,\s*timeout\s*=\s*"
                rf"{EXAMPLE_OVERALL_DEADLINE}\s*/\s*1_000\s*,?\s*\)",
                sources["python"],
            )
        )
        == 1,
        "Python runtime must use the named internal deadline",
    )

    typescript = sources["typescript"]
    typescript_external = _integer_constant(
        typescript,
        EXTERNAL_PROCESS_DEADLINE,
        "typescript",
    )
    _require_contract(
        typescript_external == external_deadline
        and typescript_external >= 2 * transport_deadlines["typescript"],
        "TypeScript external deadline no longer covers work plus cleanup",
    )
    _require_contract(
        len(
            re.findall(
                rf"{EXTERNAL_PROCESS_DEADLINE}\s*>=\s*2\s*\*\s*"
                rf"{TRANSPORT_DEADLINE}",
                typescript,
            )
        )
        == 1,
        "TypeScript must verify its operation-plus-cleanup process budget",
    )
    _require_contract(
        "JavaScript promises do not provide structured cancellation" in typescript
        and "The package gate therefore owns the honest whole-process deadline"
        in typescript,
        "TypeScript must document why it cannot claim a deceptive internal timeout",
    )


class RecordingExecutor:
    """Small command seam used to prove cleanup without invoking Docker."""

    def __init__(self, *, fail_cleanup: bool = False) -> None:
        self.commands: list[tuple[str, ...]] = []
        self.fail_cleanup = fail_cleanup

    def run(
        self,
        command: list[str] | tuple[str, ...],
        *,
        timeout: int,
        check: bool = True,
        **_: object,
    ) -> MODULE.CommandResult:
        del check
        self.commands.append(tuple(command))
        if self.fail_cleanup and tuple(command[:3]) == ("docker", "rm", "--force"):
            return MODULE.CommandResult(1, "", "injected cleanup failure")
        return MODULE.CommandResult(0, "", "")


class StoppedCopyExecutor:
    """Model only Docker's create/inspect/cp/rm path; reject execution."""

    def __init__(self, image_roots: dict[str, Path]) -> None:
        self.image_roots = image_roots
        self.containers: dict[str, str] = {}
        self.commands: list[tuple[str, ...]] = []

    def run(
        self,
        command: list[str] | tuple[str, ...],
        *,
        timeout: int,
        check: bool = True,
        **_: object,
    ) -> MODULE.CommandResult:
        del timeout, check
        values = tuple(command)
        self.commands.append(values)
        if values[0] != "docker" or values[1] in ("exec", "run", "start"):
            raise AssertionError(f"unexpected executable command: {values}")
        if values[1] == "create":
            name = values[3]
            image_id = values[4]
            self.containers[name] = image_id
            return MODULE.CommandResult(0, name, "")
        if values[1] == "inspect":
            name = values[2]
            if values[-1] == "{{.Image}}":
                return MODULE.CommandResult(0, self.containers[name] + "\n", "")
            if values[-1] == "{{.State.Running}}":
                return MODULE.CommandResult(0, "false\n", "")
        if values[1] == "cp":
            container_name, absolute_path = values[2].split(":", 1)
            source = self.image_roots[self.containers[container_name]].joinpath(
                *Path(absolute_path).parts[1:]
            )
            target = Path(values[3])
            if source.is_dir():
                shutil.copytree(source, target, symlinks=True)
            else:
                shutil.copy2(source, target, follow_symlinks=False)
            return MODULE.CommandResult(0, "", "")
        if values[1:4] == ("rm", "--force", "--volumes"):
            self.containers.pop(values[4])
            return MODULE.CommandResult(0, "", "")
        raise AssertionError(f"unexpected Docker command: {values}")


class PublicQuickstartContractTests(unittest.TestCase):
    """Prove identity, artifact, auth-gate, and cleanup primitives."""

    @staticmethod
    def _write_executable(path: Path, contents: str = "#!/bin/sh\nexit 0\n") -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.parent.chmod(0o755)
        path.write_text(contents, encoding="utf-8")
        path.chmod(0o755)

    @staticmethod
    def _node_probe_processes() -> set[int]:
        observed: set[int] = set()
        for process in Path("/proc").iterdir():
            if not process.name.isdigit():
                continue
            try:
                command = (process / "cmdline").read_bytes()
            except OSError:
                continue
            arguments = command.split(b"\0")
            if any(
                argument.startswith(b"/tmp/xenoteer-node-probe-")
                and argument.endswith(b"/node")
                for argument in arguments
            ):
                observed.add(int(process.name))
        return observed

    @staticmethod
    def _cleanup_probe_process_group(pid_file: Path, expected_root: Path) -> None:
        try:
            pid = int(pid_file.read_text(encoding="ascii"))
            command = Path(f"/proc/{pid}/cmdline").read_bytes()
            process_group = os.getpgid(pid)
        except (OSError, ValueError):
            return
        if (
            str(expected_root).encode() not in command
            or process_group <= 1
            or process_group == os.getpgrp()
        ):
            return
        try:
            os.killpg(process_group, signal.SIGKILL)
        except ProcessLookupError:
            return
        deadline = time.monotonic() + 1
        while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
            time.sleep(0.02)

    def _install_canonical_nvm_toolchain(
        self,
        root: Path,
        *,
        version: str = "v24.18.0",
    ) -> SimpleNamespace:
        home = root / "home"
        version_root = home / ".nvm/versions/node" / version
        bin_directory = version_root / "bin"
        bin_directory.mkdir(parents=True)
        for directory in (
            home,
            home / ".nvm",
            home / ".nvm/versions",
            home / ".nvm/versions/node",
            version_root,
        ):
            directory.chmod(0o775)
        node = bin_directory / "node"
        self._write_executable(
            node,
            f"#!/bin/sh\nprintf '%s\\n' {version}\n",
        )
        npm_target = version_root / "lib/node_modules/npm/bin/npm-cli.js"
        npm_target.parent.mkdir(parents=True)
        npm_target.write_bytes(b"#!/usr/bin/env node\nprocess.exitCode = 0;\n")
        npm_target.chmod(0o775)
        npm = bin_directory / "npm"
        npm.symlink_to(os.path.relpath(npm_target, bin_directory))
        identity = MODULE.BuildIdentity(
            os.getuid(),
            os.getgid(),
            home,
            False,
        )
        environment = {
            "PATH": MODULE.TRUSTED_SYSTEM_PATH,
            "XENOTEER_PACKAGE_BUILD_PATH": os.pathsep.join(
                (str(bin_directory), MODULE.TRUSTED_SYSTEM_PATH)
            ),
        }
        return SimpleNamespace(
            home=home,
            version_root=version_root,
            bin=bin_directory,
            node=node,
            npm=npm,
            npm_target=npm_target,
            identity=identity,
            environment=environment,
        )

    def test_build_identity_preserves_a_sanitized_user_toolchain_across_sudo(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer tool path ") as temporary:
            root = Path(temporary)
            tool_directory = root / "user tools"
            system_directory = root / "system-tools"
            self._write_executable(
                tool_directory / "npm-cli.js",
                "#!/usr/bin/env node\n",
            )
            (tool_directory / "npm").symlink_to("npm-cli.js")
            self._write_executable(tool_directory / "node")
            self._write_executable(system_directory / "cargo")
            (tool_directory / "npm-cli.js").chmod(0o775)
            tool_directory.chmod(0o775)
            duplicate = root / "tool-alias"
            duplicate.symlink_to(tool_directory, target_is_directory=True)
            identity = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                Path.home(),
                True,
            )
            source_environment = {
                "PATH": os.pathsep.join(
                    (
                        str(tool_directory),
                        str(duplicate),
                        str(tool_directory),
                        str(system_directory),
                    )
                ),
                "PHASE6_SECRET": "must-not-cross-privilege-drop",
            }

            command = identity.command(
                ["npm", "run", "build"],
                source_environment=source_environment,
            )

            self.assertEqual(
                command[:6],
                [
                    str(MODULE.SUDO_BINARY),
                    "-H",
                    "-u",
                    f"#{os.getuid()}",
                    "--",
                    str(MODULE.ENV_BINARY),
                ],
            )
            self.assertIn("-i", command)
            self.assertIn(f"HOME={Path.home()}", command)
            path_assignment = next(value for value in command if value.startswith("PATH="))
            path_entries = path_assignment.removeprefix("PATH=").split(os.pathsep)
            self.assertEqual(
                path_entries,
                [str(tool_directory.resolve()), str(system_directory.resolve())],
            )
            self.assertEqual(
                command[-3:],
                [str(tool_directory / "npm"), "run", "build"],
            )
            self.assertNotIn("PHASE6_SECRET", " ".join(command))

            node = identity.resolve_executable(
                "node",
                source_environment=source_environment,
            )
            cargo = identity.resolve_executable(
                "cargo",
                source_environment=source_environment,
            )
            self.assertEqual(node, (tool_directory / "node").resolve())
            self.assertEqual(cargo, (system_directory / "cargo").resolve())

    def test_build_identity_rejects_missing_malformed_and_untrusted_tool_paths(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-tools-") as temporary:
            root = Path(temporary)
            trusted = root / "trusted"
            untrusted = root / "world-writable"
            missing = root / "missing"
            self._write_executable(trusted / "npm")
            self._write_executable(untrusted / "npm")
            untrusted.chmod(0o777)
            identity = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                Path.home(),
                True,
            )
            invalid_environments = (
                {"PATH": ""},
                {"PATH": "relative/bin"},
                {"PATH": os.pathsep.join((str(trusted), "", "/usr/bin"))},
                {"PATH": str(missing)},
                {"PATH": str(untrusted)},
            )
            for environment in invalid_environments:
                with self.subTest(environment=environment):
                    with self.assertRaises(MODULE.GateError):
                        identity.command(
                            ["npm", "run", "build"],
                            source_environment=environment,
                        )

            command = identity.command(
                ["npm", "run", "build"],
                source_environment={
                    "PATH": os.pathsep.join(
                        (str(missing), str(untrusted), str(trusted))
                    )
                },
            )
            path_assignment = next(value for value in command if value.startswith("PATH="))
            self.assertEqual(
                path_assignment,
                f"PATH={trusted.resolve()}",
            )
            self.assertNotIn(str(missing), " ".join(command))
            self.assertNotIn(str(untrusted), " ".join(command))

    def test_build_identity_validates_home_and_fixed_command_wrappers(self) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-tools-") as temporary:
            root = Path(temporary)
            tool_directory = root / "bin"
            self._write_executable(tool_directory / "python")
            source_environment = {"PATH": str(tool_directory)}

            missing_home = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                root / "missing-home",
                False,
            )
            with self.assertRaises(MODULE.GateError):
                missing_home.command(
                    ["python", "-m", "build"],
                    source_environment=source_environment,
                )

            world_writable_home = root / "world-writable-home"
            world_writable_home.mkdir()
            world_writable_home.chmod(0o777)
            untrusted_home = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                world_writable_home,
                False,
            )
            with self.assertRaises(MODULE.GateError):
                untrusted_home.command(
                    ["python", "-m", "build"],
                    source_environment=source_environment,
                )

            identity = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                Path.home(),
                False,
            )
            with mock.patch.object(MODULE, "NICE_BINARY", root / "missing-nice"):
                with self.assertRaisesRegex(
                    MODULE.GateError,
                    "wrapper is unavailable: missing-nice",
                ):
                    identity.command(
                        ["python", "-m", "build"],
                        source_environment=source_environment,
                    )

    def test_build_identity_checks_permissions_as_the_target_identity(self) -> None:
        identity = MODULE.BuildIdentity(
            12_345,
            23_456,
            Path("/unused"),
            True,
        )
        root_only = mock.Mock(
            st_uid=0,
            st_gid=0,
            st_mode=stat.S_IFREG | 0o700,
        )
        target_owned = mock.Mock(
            st_uid=identity.uid,
            st_gid=99,
            st_mode=stat.S_IFREG | 0o700,
        )
        target_group = mock.Mock(
            st_uid=99,
            st_gid=identity.gid,
            st_mode=stat.S_IFREG | 0o050,
        )
        world_executable = mock.Mock(
            st_uid=0,
            st_gid=0,
            st_mode=stat.S_IFREG | 0o005,
        )

        self.assertFalse(identity._target_has_permission(root_only, 0o100, 0o010, 0o001))
        self.assertTrue(identity._target_has_permission(target_owned, 0o100, 0o010, 0o001))
        self.assertTrue(identity._target_has_permission(target_group, 0o100, 0o010, 0o001))
        self.assertTrue(
            identity._target_has_permission(world_executable, 0o100, 0o010, 0o001)
        )

        target_group_writable = mock.Mock(
            st_uid=identity.uid,
            st_gid=identity.gid,
            st_mode=stat.S_IFREG | 0o770,
        )
        supplemental_group_writable = mock.Mock(
            st_uid=identity.uid,
            st_gid=99,
            st_mode=stat.S_IFREG | 0o770,
        )
        other_writable = mock.Mock(
            st_uid=identity.uid,
            st_gid=identity.gid,
            st_mode=stat.S_IFREG | 0o777,
        )
        self.assertTrue(identity._target_writers_are_trusted(target_group_writable))
        self.assertFalse(identity._target_writers_are_trusted(supplemental_group_writable))
        self.assertFalse(identity._target_writers_are_trusted(other_writable))

    def test_build_identity_non_sudo_mode_is_still_explicit_and_sanitized(self) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-tools-") as temporary:
            tool_directory = Path(temporary) / "bin"
            self._write_executable(tool_directory / "python")
            identity = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                Path.home(),
                False,
            )
            command = identity.command(
                ["python", "-m", "build"],
                source_environment={
                    "PATH": str(tool_directory),
                    "PYTHONPATH": "/secret/source-tree",
                },
            )
            self.assertNotIn("sudo", command)
            self.assertEqual(command[0:2], [str(MODULE.ENV_BINARY), "-i"])
            self.assertEqual(
                command[-3:],
                [str((tool_directory / "python").resolve()), "-m", "build"],
            )
            self.assertNotIn("PYTHONPATH", " ".join(command))

    def test_build_identity_executes_an_env_node_shim_through_the_clean_path(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-tools-") as temporary:
            tool_directory = Path(temporary) / "bin"
            npm_cli = tool_directory / "npm-cli.js"
            self._write_executable(npm_cli, "#!/usr/bin/env node\n")
            self._write_executable(
                tool_directory / "node",
                "#!/bin/sh\nprintf '%s\\n' node-via-sanitized-path\n",
            )
            npm_cli.chmod(0o775)
            tool_directory.chmod(0o775)
            (tool_directory / "npm").symlink_to("npm-cli.js")
            identity = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                Path.home(),
                False,
            )

            completed = subprocess.run(
                identity.command(
                    ["npm", "--version"],
                    source_environment={"PATH": str(tool_directory)},
                ),
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, "node-via-sanitized-path\n")

    def test_package_build_path_channel_is_isolated_from_root_path(self) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-tools-") as temporary:
            root = Path(temporary)
            package_bin = root / "package-bin"
            system_bin = root / "system-bin"
            self._write_executable(package_bin / "node")
            self._write_executable(package_bin / "npm")
            self._write_executable(system_bin / "node")
            identity = MODULE.BuildIdentity(
                os.getuid(),
                os.getgid(),
                Path.home(),
                False,
            )
            environment = {
                "PATH": str(system_bin),
                "XENOTEER_PACKAGE_BUILD_PATH": str(package_bin),
            }

            node = identity.resolve_executable(
                "node",
                source_environment=environment,
            )
            npm = identity.resolve_executable(
                "npm",
                source_environment=environment,
            )

            self.assertEqual(node, package_bin / "node")
            self.assertEqual(npm, package_bin / "npm")
            self.assertEqual(
                identity.resolve_executable(
                    "node",
                    source_environment={"PATH": str(system_bin)},
                ),
                system_bin / "node",
            )

    def test_root_default_executor_forces_system_path_and_drops_package_path(
        self,
    ) -> None:
        completed = subprocess.CompletedProcess(
            ["docker", "version"],
            0,
            stdout="",
            stderr="",
        )
        completed_bytes = subprocess.CompletedProcess(
            ["git", "status"],
            0,
            stdout=b"",
            stderr=b"",
        )
        hostile_environment = {
            "PATH": "/home/builder/.nvm/versions/node/v24.18.0/bin",
            "XENOTEER_PACKAGE_BUILD_PATH": (
                "/home/builder/.nvm/versions/node/v24.18.0/bin"
            ),
            "BASH_ENV": "/tmp/attacker.sh",
            "DOCKER_CONFIG": "/tmp/docker",
            "DOCKER_HOST": "tcp://attacker.invalid:2375",
            "ENV": "/tmp/attacker.sh",
            "GIT_CONFIG_GLOBAL": "/tmp/attacker.gitconfig",
            "LD_PRELOAD": "/tmp/attacker.so",
            "PYTHONPATH": "/tmp/attacker-python",
            "ROOT_NON_PATH_CANARY": "must-not-cross",
            "TMPDIR": "/tmp/attacker-tmp",
        }
        with (
            mock.patch.dict(MODULE.os.environ, hostile_environment, clear=True),
            mock.patch.object(
                MODULE.subprocess,
                "run",
                side_effect=(completed, completed_bytes),
            ) as run,
        ):
            MODULE.CommandExecutor().run(
                ["docker", "version"],
                timeout=5,
            )
            MODULE.CommandExecutor().run_bytes(
                ["git", "status"],
                timeout=5,
            )

        self.assertEqual(run.call_count, 2)
        for invocation in run.call_args_list:
            environment = invocation.kwargs["env"]
            self.assertEqual(
                environment["PATH"],
                MODULE.TRUSTED_SYSTEM_PATH,
            )
            self.assertEqual(
                set(environment),
                {"HOME", "LANG", "LC_ALL", "LOGNAME", "PATH", "USER"},
            )
            self.assertNotIn(
                "XENOTEER_PACKAGE_BUILD_PATH",
                environment,
            )

    def test_source_identity_executor_routes_text_and_binary_git_as_invoking_identity(
        self,
    ) -> None:
        identity = MODULE.BuildIdentity(
            12_345,
            23_456,
            Path("/home/builder"),
            True,
        )
        text_result = subprocess.CompletedProcess(
            ["/usr/bin/git", "rev-parse", "--verify", "HEAD"],
            0,
            stdout=("a1" * 20) + "\n",
            stderr="",
        )
        binary_output = b"\xff\x00diff --git a/file b/file\n"
        binary_result = subprocess.CompletedProcess(
            ["/usr/bin/git", "diff"],
            0,
            stdout=binary_output,
            stderr=b"",
        )
        hostile_environment = {
            "HOME": "/root",
            "PATH": "/tmp/attacker-bin",
            "GIT_CONFIG": "/tmp/attacker.gitconfig",
            "GIT_CONFIG_GLOBAL": "/tmp/attacker-global.gitconfig",
            "GIT_CONFIG_SYSTEM": "/tmp/attacker-system.gitconfig",
            "GIT_DIR": "/tmp/attacker-repository",
            "GIT_WORK_TREE": "/tmp/attacker-worktree",
            "XENOTEER_TOKEN": "must-not-cross",
        }
        account = SimpleNamespace(
            pw_uid=identity.uid,
            pw_gid=identity.gid,
            pw_dir=str(identity.home),
            pw_name="builder",
        )

        with (
            mock.patch.dict(MODULE.os.environ, hostile_environment, clear=True),
            mock.patch.object(MODULE.os, "geteuid", return_value=0),
            mock.patch.object(MODULE.pwd, "getpwuid", return_value=account),
            mock.patch.object(
                MODULE.subprocess,
                "run",
                side_effect=(text_result, binary_result),
            ) as run,
        ):
            source_executor = MODULE.SourceIdentityExecutor(
                identity,
                MODULE.CommandExecutor(),
                Path("/usr/bin/git"),
            )
            text = source_executor.run(
                ["git", "rev-parse", "--verify", "HEAD"],
                timeout=5,
                cwd=Path("/repository"),
            )
            binary = source_executor.run_bytes(
                ["git", "diff"],
                timeout=5,
                cwd=Path("/repository"),
            )

        self.assertEqual(text.stdout, text_result.stdout)
        self.assertEqual(binary, binary_output)
        self.assertEqual(run.call_count, 2)
        expected_environment = {
            "HOME": str(identity.home),
            "LANG": "C",
            "LC_ALL": "C",
            "LOGNAME": account.pw_name,
            "PATH": MODULE.TRUSTED_SYSTEM_PATH,
            "USER": account.pw_name,
        }
        for invocation in run.call_args_list:
            arguments, keywords = invocation
            self.assertEqual(arguments[0][0], "/usr/bin/git")
            self.assertNotIn("-c", arguments[0])
            self.assertNotIn("safe.directory", arguments[0])
            self.assertEqual(keywords["cwd"], Path("/repository"))
            self.assertEqual(keywords["env"], expected_environment)
            self.assertEqual(keywords["user"], identity.uid)
            self.assertEqual(keywords["group"], identity.gid)
            self.assertEqual(keywords["extra_groups"], ())
            self.assertNotIn("XENOTEER_TOKEN", keywords["env"])
            self.assertFalse(
                any(key.startswith("GIT_") for key in keywords["env"])
            )
        self.assertTrue(run.call_args_list[0].kwargs["text"])
        self.assertFalse(run.call_args_list[1].kwargs["text"])

    def test_source_identity_executor_rejects_invalid_identity_command_and_bounds(
        self,
    ) -> None:
        identity = MODULE.BuildIdentity(
            12_345,
            23_456,
            Path("/home/builder"),
            True,
        )
        account = SimpleNamespace(
            pw_uid=identity.uid,
            pw_gid=identity.gid,
            pw_dir=str(identity.home),
            pw_name="builder",
        )
        with mock.patch.object(MODULE.pwd, "getpwuid", return_value=account):
            source_executor = MODULE.SourceIdentityExecutor(
                identity,
                MODULE.CommandExecutor(),
                Path("/usr/bin/git"),
            )
        invalid_calls = (
            lambda: source_executor.run([], timeout=5, cwd=Path("/repository")),
            lambda: source_executor.run(
                ["docker", "version"],
                timeout=5,
                cwd=Path("/repository"),
            ),
            lambda: source_executor.run(
                ["git", "status"],
                timeout=0,
                cwd=Path("/repository"),
            ),
            lambda: source_executor.run(
                ["git", "status"],
                timeout=5,
                cwd=None,
            ),
            lambda: source_executor.run(
                ["git", "status"],
                timeout=5,
                cwd=Path("relative"),
            ),
            lambda: source_executor.run(
                ["git", "status\0--short"],
                timeout=5,
                cwd=Path("/repository"),
            ),
        )
        for invalid_call in invalid_calls:
            with self.subTest(call=invalid_call):
                with self.assertRaises(MODULE.GateError):
                    invalid_call()

    def test_source_identity_executor_rejects_root_and_mismatched_local_accounts(
        self,
    ) -> None:
        with self.assertRaisesRegex(MODULE.GateError, "non-root"):
            MODULE.SourceIdentityExecutor(
                MODULE.BuildIdentity(0, 0, Path("/root"), False),
                MODULE.CommandExecutor(),
                Path("/usr/bin/git"),
            )

        identity = MODULE.BuildIdentity(
            max(os.geteuid(), 1),
            max(os.getegid(), 1),
            Path("/home/builder"),
            False,
        )
        mismatches = (
            SimpleNamespace(
                pw_uid=identity.uid + 1,
                pw_gid=identity.gid,
                pw_dir=str(identity.home),
                pw_name="builder",
            ),
            SimpleNamespace(
                pw_uid=identity.uid,
                pw_gid=identity.gid + 1,
                pw_dir=str(identity.home),
                pw_name="builder",
            ),
            SimpleNamespace(
                pw_uid=identity.uid,
                pw_gid=identity.gid,
                pw_dir="/root",
                pw_name="builder",
            ),
            SimpleNamespace(
                pw_uid=identity.uid,
                pw_gid=identity.gid,
                pw_dir=str(identity.home),
                pw_name="",
            ),
        )
        for account in mismatches:
            with (
                self.subTest(account=account),
                mock.patch.object(MODULE.pwd, "getpwuid", return_value=account),
                self.assertRaises(MODULE.GateError),
            ):
                MODULE.SourceIdentityExecutor(
                    identity,
                    MODULE.CommandExecutor(),
                    Path("/usr/bin/git"),
                )

        with (
            mock.patch.object(MODULE.pwd, "getpwuid", side_effect=KeyError),
            self.assertRaisesRegex(MODULE.GateError, "no local account"),
        ):
            MODULE.SourceIdentityExecutor(
                identity,
                MODULE.CommandExecutor(),
                Path("/usr/bin/git"),
            )

    def test_source_identity_binary_boundary_has_typed_failure_paths(self) -> None:
        identity = MODULE.BuildIdentity(
            12_345,
            23_456,
            Path("/home/builder"),
            True,
        )
        account = SimpleNamespace(
            pw_uid=identity.uid,
            pw_gid=identity.gid,
            pw_dir=str(identity.home),
            pw_name="builder",
        )
        with mock.patch.object(MODULE.pwd, "getpwuid", return_value=account):
            source_executor = MODULE.SourceIdentityExecutor(
                identity,
                MODULE.CommandExecutor(),
                Path("/usr/bin/git"),
            )
        failures = (
            FileNotFoundError("missing"),
            PermissionError("denied"),
            OSError("launch failed"),
            ValueError("embedded null byte"),
            subprocess.TimeoutExpired(["/usr/bin/git"], 5),
            subprocess.CompletedProcess(["/usr/bin/git"], 1, b"", b"\xff"),
            subprocess.CompletedProcess(["/usr/bin/git"], 0, "not bytes", b""),
        )
        for failure in failures:
            with self.subTest(failure=type(failure).__name__):
                with (
                    mock.patch.object(MODULE.os, "geteuid", return_value=0),
                    mock.patch.object(
                        MODULE.subprocess,
                        "run",
                        side_effect=failure
                        if isinstance(failure, BaseException)
                        else None,
                        return_value=None
                        if isinstance(failure, BaseException)
                        else failure,
                    ),
                    self.assertRaises(MODULE.GateError),
                ):
                    source_executor.run_bytes(
                        ["git", "diff"],
                        timeout=5,
                        cwd=Path("/repository"),
                    )

    def test_system_git_resolution_ignores_the_package_build_path(self) -> None:
        identity = MODULE.BuildIdentity(
            os.geteuid(),
            os.getegid(),
            Path.home(),
            False,
        )
        with tempfile.TemporaryDirectory(prefix="xenoteer-fake-git-") as temporary:
            fake_git = Path(temporary) / "git"
            self._write_executable(fake_git, "#!/bin/sh\nexit 99\n")
            resolved = identity.resolve_system_executable(
                "git",
                source_environment={
                    "PATH": str(fake_git.parent),
                    "XENOTEER_PACKAGE_BUILD_PATH": str(fake_git.parent),
                },
            )

        self.assertEqual(resolved, Path("/usr/bin/git"))

    def test_system_git_resolution_has_no_untrusted_fallback(self) -> None:
        identity = MODULE.BuildIdentity(
            os.geteuid(),
            os.getegid(),
            Path.home(),
            False,
        )
        with tempfile.TemporaryDirectory(prefix="xenoteer-untrusted-git-") as temporary:
            directory = Path(temporary)
            fake_git = directory / "git"
            self._write_executable(fake_git, "#!/bin/sh\nexit 0\n")
            fake_git.chmod(0o777)
            with (
                mock.patch.object(
                    MODULE,
                    "TRUSTED_SYSTEM_PATH",
                    str(directory),
                ),
                self.assertRaisesRegex(
                    MODULE.GateError,
                    "required system executable is unavailable",
                ),
            ):
                identity.resolve_system_executable(
                    "git",
                    source_environment={
                        "PATH": str(directory),
                        "XENOTEER_PACKAGE_BUILD_PATH": str(directory),
                    },
                )

    def test_real_source_identity_boundary_preserves_binary_and_untracked_inputs(
        self,
    ) -> None:
        if os.geteuid() == 0:
            self.skipTest("the harmless real boundary seam requires non-root")
        with tempfile.TemporaryDirectory(prefix="xenoteer-git-boundary-") as temporary:
            repository = Path(temporary)
            subprocess.run(
                ["/usr/bin/git", "init", "--quiet", str(repository)],
                check=True,
                timeout=5,
            )
            tracked = repository / "tracked.bin"
            tracked.write_bytes(b"baseline\n")
            subprocess.run(
                ["/usr/bin/git", "-C", str(repository), "add", "tracked.bin"],
                check=True,
                timeout=5,
            )
            commit_environment = dict(os.environ)
            commit_environment.update(
                {
                    "GIT_AUTHOR_NAME": "Xenoteer Test",
                    "GIT_AUTHOR_EMAIL": "xenoteer@example.invalid",
                    "GIT_COMMITTER_NAME": "Xenoteer Test",
                    "GIT_COMMITTER_EMAIL": "xenoteer@example.invalid",
                }
            )
            subprocess.run(
                ["/usr/bin/git", "-C", str(repository), "commit", "--quiet", "-m", "base"],
                check=True,
                env=commit_environment,
                timeout=5,
            )
            tracked.write_bytes(b"baseline\n\x00\xffchanged\n")
            untracked = repository / "untracked.bin"
            untracked.write_bytes(b"\x00\xffuntracked\n")
            untracked.chmod(0o600)
            identity = MODULE.BuildIdentity.current()
            source_executor = MODULE.SourceIdentityExecutor(
                identity,
                MODULE.CommandExecutor(),
                identity.resolve_system_executable("git"),
            )

            observed = MODULE.current_source_tree_hash(
                repository,
                source_executor,
            )
            expected_diff = subprocess.run(
                [
                    "/usr/bin/git",
                    "-C",
                    str(repository),
                    "diff",
                    "--binary",
                    "--no-ext-diff",
                    "HEAD",
                    "--",
                ],
                check=True,
                capture_output=True,
                timeout=5,
            ).stdout
            head = subprocess.run(
                ["/usr/bin/git", "-C", str(repository), "rev-parse", "--verify", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
                timeout=5,
            ).stdout.strip()
            expected = MODULE.source_snapshot_digest(
                head,
                expected_diff,
                (
                    MODULE.UntrackedSource(
                        "untracked.bin",
                        "600",
                        MODULE.file_sha256(untracked),
                    ),
                ),
            )

        self.assertEqual(observed, expected)

    def test_fake_docker_in_package_path_cannot_execute_at_root_boundary(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-root-path-") as temporary:
            root = Path(temporary)
            package_bin = root / "package-bin"
            system_bin = root / "system-bin"
            marker = root / "hijacked"
            self._write_executable(
                package_bin / "docker",
                f"#!/bin/sh\n/usr/bin/touch {marker}\n",
            )
            system_bin.mkdir()
            environment = {
                "PATH": str(package_bin),
                "XENOTEER_PACKAGE_BUILD_PATH": str(package_bin),
            }

            with (
                mock.patch.dict(MODULE.os.environ, environment, clear=True),
                mock.patch.object(
                    MODULE,
                    "TRUSTED_SYSTEM_PATH",
                    str(system_bin),
                ),
                self.assertRaisesRegex(
                    MODULE.GateError,
                    "required command is unavailable: docker",
                ),
            ):
                MODULE.CommandExecutor().run(
                    ["docker", "version"],
                    timeout=5,
                )

            self.assertFalse(marker.exists())

    def test_canonical_nvm_toolchain_is_revalidated_once_at_package_use(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            installed = self._install_canonical_nvm_toolchain(Path(temporary))

            toolchain = MODULE.resolve_package_toolchain(
                installed.identity,
                MODULE.CommandExecutor(),
                source_environment=installed.environment,
            )

            self.assertEqual(toolchain.node, installed.node)
            self.assertEqual(toolchain.npm, installed.npm)
            self.assertEqual(toolchain.version, (24, 18, 0))
            self.assertEqual(
                toolchain.path,
                tuple(
                    installed.identity._trusted_path(installed.environment)
                ),
            )

    def test_post_qualifier_node_runtime_mutation_fails_at_package_use(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            installed = self._install_canonical_nvm_toolchain(Path(temporary))
            self._write_executable(
                installed.node,
                "#!/bin/sh\nprintf '%s\\n' v22.19.1\n",
            )

            with self.assertRaisesRegex(
                MODULE.GateError,
                "does not match selected NVM version",
            ):
                MODULE.resolve_package_toolchain(
                    installed.identity,
                    MODULE.CommandExecutor(),
                    source_environment=installed.environment,
                )

    def test_selected_package_toolchain_never_falls_back_after_mutation(
        self,
    ) -> None:
        for case in ("delete-node", "replace-bin-with-alias"):
            with self.subTest(case=case), tempfile.TemporaryDirectory(
                prefix="xenoteer-nvm-use-"
            ) as temporary:
                root = Path(temporary)
                installed = self._install_canonical_nvm_toolchain(root)
                fallback = root / "fallback/bin"
                self._write_executable(
                    fallback / "node",
                    "#!/bin/sh\nprintf '%s\\n' v22.19.1\n",
                )
                self._write_executable(fallback / "npm")
                installed.environment["XENOTEER_PACKAGE_BUILD_PATH"] = (
                    f"{installed.bin}{os.pathsep}{fallback}"
                )
                if case == "delete-node":
                    installed.node.unlink()
                else:
                    replacement = installed.version_root / "original-bin"
                    installed.bin.rename(replacement)
                    installed.bin.symlink_to(fallback, target_is_directory=True)

                with self.assertRaisesRegex(
                    MODULE.GateError,
                    "selected package toolchain",
                ):
                    MODULE.resolve_package_toolchain(
                        installed.identity,
                        MODULE.CommandExecutor(),
                        source_environment=installed.environment,
                    )

    def test_nvm_selection_rejects_a_raw_path_alias(self) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            root = Path(temporary)
            installed = self._install_canonical_nvm_toolchain(root)
            alias = root / "nvm-bin-alias"
            alias.symlink_to(installed.bin, target_is_directory=True)
            installed.environment["XENOTEER_PACKAGE_BUILD_PATH"] = str(alias)

            with self.assertRaisesRegex(MODULE.GateError, "alias"):
                MODULE.resolve_package_toolchain(
                    installed.identity,
                    MODULE.CommandExecutor(),
                    source_environment=installed.environment,
                )

    def test_canonical_nvm_node_must_be_a_regular_executable(self) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            installed = self._install_canonical_nvm_toolchain(Path(temporary))
            node_target = installed.version_root / "node-target"
            installed.node.rename(node_target)
            installed.node.symlink_to(node_target)

            with self.assertRaisesRegex(
                MODULE.GateError,
                "NVM node must be a regular executable",
            ):
                MODULE.resolve_package_toolchain(
                    installed.identity,
                    MODULE.CommandExecutor(),
                    source_environment=installed.environment,
                )

    def test_canonical_nvm_npm_requires_a_trusted_in_root_symlink(self) -> None:
        cases = ("regular", "dangling", "outside")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory(
                prefix="xenoteer-nvm-use-"
            ) as temporary:
                installed = self._install_canonical_nvm_toolchain(Path(temporary))
                installed.npm.unlink()
                if case == "regular":
                    self._write_executable(
                        installed.npm,
                        "#!/usr/bin/env node\n",
                    )
                elif case == "dangling":
                    installed.npm.symlink_to("../missing/npm-cli.js")
                else:
                    outside = Path(temporary) / "outside/npm-cli.js"
                    self._write_executable(
                        outside,
                        "#!/usr/bin/env node\n",
                    )
                    installed.npm.symlink_to(outside)

                with self.assertRaisesRegex(
                    MODULE.GateError,
                    "NVM npm must be a trusted in-root symlink",
                ):
                    MODULE.resolve_package_toolchain(
                        installed.identity,
                        MODULE.CommandExecutor(),
                        source_environment=installed.environment,
                    )

    def test_canonical_nvm_npm_rejects_an_in_root_intermediate_symlink(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            installed = self._install_canonical_nvm_toolchain(Path(temporary))
            npm_bin = installed.npm_target.parent
            npm_target_contents = installed.npm_target.read_bytes()
            installed.npm_target.unlink()
            npm_bin.rmdir()
            actual_bin = installed.version_root / "actual-npm-bin"
            actual_bin.mkdir()
            (actual_bin / "npm-cli.js").write_bytes(npm_target_contents)
            (actual_bin / "npm-cli.js").chmod(0o775)
            npm_bin.symlink_to(
                os.path.relpath(actual_bin, npm_bin.parent),
                target_is_directory=True,
            )

            with self.assertRaisesRegex(
                MODULE.GateError,
                "NVM npm target path components",
            ):
                MODULE.resolve_package_toolchain(
                    installed.identity,
                    MODULE.CommandExecutor(),
                    source_environment=installed.environment,
                )

    def test_canonical_nvm_npm_rejects_an_intermediate_component_race(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            installed = self._install_canonical_nvm_toolchain(Path(temporary))
            original_open = os.open
            lib = installed.version_root / "lib"
            raced_lib = installed.version_root / "lib-before-race"
            raced = False

            def racing_open(
                path: object,
                flags: int,
                mode: int = 0o777,
                *,
                dir_fd: int | None = None,
            ) -> int:
                nonlocal raced
                descriptor = original_open(path, flags, mode, dir_fd=dir_fd)
                if Path(path) == installed.version_root and not raced:
                    raced = True
                    lib.rename(raced_lib)
                    lib.symlink_to(raced_lib, target_is_directory=True)
                return descriptor

            with (
                mock.patch.object(MODULE.os, "open", side_effect=racing_open),
                self.assertRaisesRegex(
                    MODULE.GateError,
                    "NVM npm target path components",
                ),
            ):
                MODULE.resolve_package_toolchain(
                    installed.identity,
                    MODULE.CommandExecutor(),
                    source_environment=installed.environment,
                )
            self.assertTrue(raced)

    def test_canonical_nvm_npm_requires_exact_bounded_env_node_shebang(
        self,
    ) -> None:
        invalid_targets = (
            b"#!/usr/bin/node\n",
            b"#!/usr/bin/env node\r\n",
            b"\xef\xbb\xbf#!/usr/bin/env node\n",
            b"#!/usr/bin/env node --no-warnings\n",
            b"#!/usr/bin/env node",
            b"#!/usr/bin/env node\n" + (b"x" * (64 * 1024)),
        )
        for contents in invalid_targets:
            with self.subTest(prefix=contents[:32]), tempfile.TemporaryDirectory(
                prefix="xenoteer-nvm-use-"
            ) as temporary:
                installed = self._install_canonical_nvm_toolchain(Path(temporary))
                installed.npm_target.write_bytes(contents)
                installed.npm_target.chmod(0o775)

                with self.assertRaisesRegex(
                    MODULE.GateError,
                    "NVM npm wrapper",
                ):
                    MODULE.resolve_package_toolchain(
                        installed.identity,
                        MODULE.CommandExecutor(),
                        source_environment=installed.environment,
                    )

    def test_canonical_nvm_npm_special_target_fails_without_blocking(self) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-nvm-use-") as temporary:
            installed = self._install_canonical_nvm_toolchain(Path(temporary))
            installed.npm_target.unlink()
            os.mkfifo(installed.npm_target, mode=0o700)

            started = time.monotonic()
            with self.assertRaisesRegex(
                MODULE.GateError,
                "NVM npm wrapper",
            ):
                MODULE.resolve_package_toolchain(
                    installed.identity,
                    MODULE.CommandExecutor(),
                    source_environment=installed.environment,
                )
            self.assertLess(time.monotonic() - started, 1)

    def test_node_probe_rejects_stdout_stderr_floods_and_invalid_utf8(self) -> None:
        scripts = {
            "stdout": "#!/bin/sh\nwhile :; do printf 'xxxxxxxxxxxxxxxx'; done\n",
            "stderr": (
                "#!/bin/sh\nwhile :; do "
                "printf 'xxxxxxxxxxxxxxxx' >&2; done\n"
            ),
            "invalid-utf8": "#!/bin/sh\nprintf '\\377'\n",
        }
        for case, contents in scripts.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory(
                prefix="xenoteer-node-probe-"
            ) as temporary:
                executable = Path(temporary) / "node"
                self._write_executable(executable, contents)
                message = "output limit" if case != "invalid-utf8" else "invalid UTF-8"
                with self.assertRaisesRegex(MODULE.GateError, message):
                    MODULE.CommandExecutor().run_probe(
                        [str(executable)],
                        timeout=1,
                        output_limit=128,
                    )

    def test_node_probe_converts_exec_race_oserror_to_typed_failure(self) -> None:
        with mock.patch.object(
            MODULE.subprocess,
            "Popen",
            side_effect=OSError(8, "injected executable replacement race"),
        ):
            with self.assertRaisesRegex(
                MODULE.GateError,
                "node version probe",
            ):
                MODULE.CommandExecutor().run_probe(
                    ["/tmp/replaced-node"],
                    timeout=1,
                    output_limit=128,
                )

    def test_node_probe_rejects_invalid_bounds_and_empty_commands(self) -> None:
        cases = (
            ((), 1, 128, "empty"),
            (("/usr/bin/true",), 0, 128, "timeout"),
            (
                ("/usr/bin/true",),
                MODULE.DEFAULT_COMMAND_TIMEOUT_SECONDS + 1,
                128,
                "timeout",
            ),
            (("/usr/bin/true",), 1, 0, "output limit"),
            (
                ("/usr/bin/true",),
                1,
                MODULE.NODE_PROBE_OUTPUT_LIMIT_BYTES + 1,
                "output limit",
            ),
        )
        for command, timeout, output_limit, message in cases:
            with self.subTest(
                command=command,
                timeout=timeout,
                output_limit=output_limit,
            ), self.assertRaisesRegex(MODULE.GateError, message):
                MODULE.CommandExecutor().run_probe(
                    command,
                    timeout=timeout,
                    output_limit=output_limit,
                )

    def test_package_toolchain_rejects_bad_node_probe_results(self) -> None:
        cases = (
            (
                "#!/bin/sh\nprintf '%s\\n' v24.18.0 >&2\n",
                "unexpected stderr",
            ),
            ("#!/bin/sh\nexit 7\n", "probe failed"),
            ("#!/bin/sh\nprintf '%s\\n' not-a-version\n", "unsupported"),
            ("#!/bin/sh\nprintf '%s\\n' v20.18.0\n", "unsupported"),
            (
                "#!/bin/sh\nprintf 'v24.18.0\\nextra\\n'\n",
                "unsupported",
            ),
        )
        for contents, message in cases:
            with self.subTest(message=message), tempfile.TemporaryDirectory(
                prefix="xenoteer-nvm-use-"
            ) as temporary:
                installed = self._install_canonical_nvm_toolchain(Path(temporary))
                self._write_executable(installed.node, contents)
                with self.assertRaisesRegex(MODULE.GateError, message):
                    MODULE.resolve_package_toolchain(
                        installed.identity,
                        MODULE.CommandExecutor(),
                        source_environment=installed.environment,
                    )

    def test_node_probe_kills_term_ignoring_pipe_holding_descendants(
        self,
    ) -> None:
        root = Path(tempfile.mkdtemp(prefix="xenoteer-node-probe-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        child_pid = root / "child.pid"
        self.addCleanup(self._cleanup_probe_process_group, child_pid, root)
        executable = root / "node"
        self._write_executable(
            executable,
            "#!/bin/sh\n"
            "trap '' TERM\n"
            "(trap '' TERM; while :; do /usr/bin/sleep 1; done) &\n"
            f"printf '%s' \"$!\" > {child_pid}\n"
            "wait\n",
        )

        started = time.monotonic()
        with self.assertRaisesRegex(MODULE.GateError, "exceeded"):
            MODULE.CommandExecutor().run_probe(
                [str(executable)],
                timeout=1,
                output_limit=128,
            )
        self.assertLess(time.monotonic() - started, 4)
        self.assertTrue(child_pid.is_file())
        pid = int(child_pid.read_text(encoding="ascii"))
        deadline = time.monotonic() + 1
        while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertFalse(Path(f"/proc/{pid}").exists())

    def test_successful_node_probe_reaps_descendants_that_close_their_pipes(
        self,
    ) -> None:
        before = self._node_probe_processes()
        root = Path(tempfile.mkdtemp(prefix="xenoteer-node-probe-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        child_pid = root / "child.pid"
        self.addCleanup(self._cleanup_probe_process_group, child_pid, root)
        executable = root / "node"
        self._write_executable(
            executable,
            "#!/bin/sh\n"
            "(trap '' TERM; exec >/dev/null 2>/dev/null; "
            "while :; do /usr/bin/sleep 1; done) &\n"
            f"printf '%s' \"$!\" > {child_pid}\n"
            "printf '%s\\n' v24.18.0\n",
        )

        result = MODULE.CommandExecutor().run_probe(
            [str(executable)],
            timeout=1,
            output_limit=128,
        )

        self.assertEqual(result.stdout, "v24.18.0\n")
        self.assertTrue(child_pid.is_file())
        pid = int(child_pid.read_text(encoding="ascii"))
        deadline = time.monotonic() + 1
        while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertFalse(Path(f"/proc/{pid}").exists())
        self.assertEqual(
            self._node_probe_processes(),
            before,
            "successful probe leaked a /tmp/xenoteer-node-probe-* process",
        )

    def test_invalid_text_from_default_command_is_typed(self) -> None:
        with mock.patch.object(
            MODULE.subprocess,
            "run",
            side_effect=UnicodeDecodeError(
                "utf-8",
                b"\xff",
                0,
                1,
                "invalid start byte",
            ),
        ):
            with self.assertRaisesRegex(MODULE.GateError, "invalid UTF-8"):
                MODULE.CommandExecutor().run(
                    ["/usr/bin/true"],
                    timeout=5,
                )

    def test_safe_command_redacts_clean_environment_home_and_path(self) -> None:
        rendered = MODULE.safe_command(
            (
                str(MODULE.ENV_BINARY),
                "-i",
                "HOME=/home/private-user",
                "PATH=/home/private-user/tools:/usr/bin",
                "/usr/bin/node",
                "--version",
            )
        )
        self.assertIn("HOME=<redacted>", rendered)
        self.assertIn("PATH=<redacted>", rendered)
        self.assertNotIn("/home/private-user", rendered)

    def test_sudo_quickstart_boundary_drops_user_group_and_supplemental_groups(
        self,
    ) -> None:
        identity = MODULE.BuildIdentity(12_345, 23_456, Path("/home/builder"), True)
        completed = subprocess.CompletedProcess(
            ["/usr/bin/true"],
            0,
            stdout="",
            stderr="",
        )
        environment = {
            "HOME": "/home/builder",
            "PATH": "/usr/bin",
            "XENOTEER_TOKEN": "bearer-secret-that-must-stay-out-of-argv",
        }

        with (
            mock.patch.object(MODULE.os, "geteuid", return_value=0),
            mock.patch.object(MODULE.subprocess, "run", return_value=completed) as run,
        ):
            result = MODULE.CommandExecutor().run_as_identity(
                ["/usr/bin/true"],
                identity=identity,
                timeout=5,
                env=environment,
            )

        self.assertEqual(result.returncode, 0)
        arguments, keywords = run.call_args
        self.assertEqual(arguments[0], ["/usr/bin/true"])
        self.assertEqual(keywords["user"], identity.uid)
        self.assertEqual(keywords["group"], identity.gid)
        self.assertEqual(keywords["extra_groups"], ())
        self.assertEqual(keywords["env"], environment)
        self.assertNotIn(environment["XENOTEER_TOKEN"], " ".join(arguments[0]))
        self.assertNotIn(
            environment["XENOTEER_TOKEN"],
            MODULE.safe_command(arguments[0]),
        )

    def test_non_sudo_quickstart_boundary_preserves_current_identity_and_exact_env(
        self,
    ) -> None:
        if os.geteuid() == 0:
            self.skipTest("the harmless rootless identity fixture requires non-root")
        identity = MODULE.BuildIdentity(
            os.geteuid(),
            os.getegid(),
            Path.home(),
            False,
        )
        environment = {
            "HOME": str(Path.home()),
            "PATH": "/usr/bin",
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "BOUNDARY_CANARY": "present",
        }
        program = (
            "import json, os; "
            "print(json.dumps({'uid': os.geteuid(), 'gid': os.getegid(), "
            "'environment': dict(os.environ)}, sort_keys=True))"
        )

        result = MODULE.CommandExecutor().run_as_identity(
            [sys.executable, "-c", program],
            identity=identity,
            timeout=5,
            env=environment,
        )

        observed = json.loads(result.stdout)
        self.assertEqual(observed["uid"], identity.uid)
        self.assertEqual(observed["gid"], identity.gid)
        self.assertEqual(observed["environment"], environment)

    def test_quickstart_boundary_rejects_root_and_mismatched_rootless_identities(
        self,
    ) -> None:
        executor = MODULE.CommandExecutor()
        with self.assertRaisesRegex(MODULE.GateError, "cannot be root"):
            executor.run_as_identity(
                ["/usr/bin/true"],
                identity=MODULE.BuildIdentity(0, 0, Path("/root"), False),
                timeout=5,
                env={},
            )

        for gid in (0, -1):
            with self.subTest(gid=gid):
                with self.assertRaisesRegex(MODULE.GateError, "target group is invalid"):
                    executor.run_as_identity(
                        ["/usr/bin/true"],
                        identity=MODULE.BuildIdentity(
                            max(os.geteuid(), 1),
                            gid,
                            Path.home(),
                            True,
                        ),
                        timeout=5,
                        env={},
                    )

        mismatched = MODULE.BuildIdentity(
            os.geteuid() + 10_000,
            os.getegid() + 10_000,
            Path("/home/not-current"),
            False,
        )
        with self.assertRaisesRegex(MODULE.GateError, "current process identity"):
            executor.run_as_identity(
                ["/usr/bin/true"],
                identity=mismatched,
                timeout=5,
                env={},
            )

    def test_quickstart_boundary_timeout_never_renders_environment_secrets(
        self,
    ) -> None:
        token = "timeout-bearer-secret"
        identity = MODULE.BuildIdentity(
            os.geteuid(),
            os.getegid(),
            Path.home(),
            False,
        )
        timeout = subprocess.TimeoutExpired(
            ["/usr/bin/true"],
            5,
            output=token,
            stderr=token,
        )
        with mock.patch.object(MODULE.subprocess, "run", side_effect=timeout):
            with self.assertRaises(MODULE.GateError) as raised:
                MODULE.CommandExecutor().run_as_identity(
                    ["/usr/bin/true"],
                    identity=identity,
                    timeout=5,
                    env={"XENOTEER_TOKEN": token},
                )
        self.assertNotIn(token, str(raised.exception))

    def test_quickstart_boundary_launch_error_never_renders_environment_secrets(
        self,
    ) -> None:
        token = "launch-error-bearer-secret"
        identity = MODULE.BuildIdentity(
            os.geteuid(),
            os.getegid(),
            Path.home(),
            False,
        )
        with mock.patch.object(
            MODULE.subprocess,
            "run",
            side_effect=PermissionError(token),
        ):
            with self.assertRaises(MODULE.GateError) as raised:
                MODULE.CommandExecutor().run_as_identity(
                    ["/usr/bin/true"],
                    identity=identity,
                    timeout=5,
                    env={"XENOTEER_TOKEN": token},
                )
        self.assertNotIn(token, str(raised.exception))

    def test_every_quickstart_variant_uses_validated_run_as_boundary_and_exact_env(
        self,
    ) -> None:
        token = "valid-bearer-secret"
        wrong_token = "wrong-bearer-secret"
        with tempfile.TemporaryDirectory(prefix="xenoteer-runtime-") as temporary:
            root = Path(temporary)
            executable = root / "installed-consumer"
            self._write_executable(executable)
            identity = MODULE.BuildIdentity(
                os.geteuid(),
                os.getegid(),
                Path.home(),
                True,
            )
            variants = (
                ("rust-crate", (str(executable),), root / "rust"),
                (
                    "npm-tarball",
                    (str(executable), str(root / "example.mjs")),
                    root / "typescript",
                ),
                (
                    "python-wheel",
                    (str(executable), "-m", "wheel_example"),
                    root / "wheel-site",
                ),
                (
                    "python-sdist",
                    (str(executable), "-m", "sdist_example"),
                    root / "sdist-site",
                ),
            )
            for _, _, installed_root in variants:
                installed_root.mkdir(parents=True)
            executor = mock.Mock()

            for name, command, installed_root in variants:
                with self.subTest(name=name):
                    executor.reset_mock()
                    output = "\n".join(
                        [
                            f"quickstart-ok language={name} behavior={behavior}"
                            for behavior in MODULE.REQUIRED_BEHAVIORS
                        ]
                        + [f"quickstart-ok language={name} mode=success", ""]
                    )
                    executor.run_as_identity.return_value = MODULE.CommandResult(
                        0,
                        output,
                        "",
                    )
                    with mock.patch.dict(
                        os.environ,
                        {
                            "AMBIENT_CANARY": "must-not-cross",
                            "HTTPS_PROXY": "http://ambient-proxy.invalid",
                            "PYTHONPATH": "/ambient/source",
                        },
                        clear=False,
                    ):
                        MODULE.run_one_quickstart(
                            executor,
                            identity=identity,
                            name=name,
                            command=command,
                            installed_root=installed_root,
                            api_base="http://127.0.0.1:43210",
                            token=token,
                            expect_auth_failure=False,
                            forbidden_tokens=(token, wrong_token),
                        )

                    executor.run.assert_not_called()
                    executor.run_as_identity.assert_called_once()
                    arguments, keywords = executor.run_as_identity.call_args
                    argv = arguments[0]
                    environment = keywords["env"]
                    self.assertEqual(keywords["identity"], identity)
                    self.assertEqual(keywords["cwd"], installed_root.resolve())
                    self.assertEqual(
                        argv[:6],
                        [
                            str(MODULE.NICE_BINARY),
                            "-n",
                            "15",
                            str(MODULE.IONICE_BINARY),
                            "-c",
                            "3",
                        ],
                    )
                    self.assertEqual(argv[6:], list(command))
                    self.assertTrue(Path(argv[6]).is_absolute())
                    self.assertNotIn(token, " ".join(argv))
                    self.assertNotIn(token, MODULE.safe_command(argv))
                    self.assertFalse(
                        any(
                            argument.startswith("XENOTEER_")
                            or argument == str(MODULE.ENV_BINARY)
                            for argument in argv
                        )
                    )
                    expected = {
                        "HOME": str(Path.home()),
                        "PATH": os.pathsep.join(
                            str(path) for path in identity._trusted_path()
                        ),
                        "LANG": "C.UTF-8",
                        "LC_ALL": "C.UTF-8",
                        "XENOTEER_API_BASE": "http://127.0.0.1:43210",
                        "XENOTEER_TOKEN": token,
                        "XENOTEER_EXPECTED_INSTALL_ROOT": str(installed_root),
                        "XENOTEER_EXPECT_AUTH_FAILURE": "0",
                        "XENOTEER_QUICKSTART_LANGUAGE": name,
                        "PYTHONNOUSERSITE": "1",
                        "RUST_BACKTRACE": "0",
                    }
                    if name.startswith("python-"):
                        expected["PYTHONPATH"] = str(installed_root)
                    self.assertEqual(environment, expected)
                    self.assertNotIn("AMBIENT_CANARY", environment)
                    self.assertNotIn("HTTPS_PROXY", environment)

    def test_quickstart_failure_redacts_raw_bearer_output(self) -> None:
        token = "stderr-bearer-secret"
        with tempfile.TemporaryDirectory(prefix="xenoteer-runtime-") as temporary:
            executable = Path(temporary) / "installed-consumer"
            self._write_executable(executable)
            identity = MODULE.BuildIdentity(
                os.geteuid(),
                os.getegid(),
                Path.home(),
                False,
            )
            executor = mock.Mock()
            executor.run_as_identity.return_value = MODULE.CommandResult(
                1,
                "",
                f"upstream accidentally printed {token}",
            )

            with self.assertRaises(MODULE.GateError) as raised:
                MODULE.run_one_quickstart(
                    executor,
                    identity=identity,
                    name="rust-crate",
                    command=(str(executable),),
                    installed_root=Path(temporary),
                    api_base="http://127.0.0.1:43210",
                    token=token,
                    expect_auth_failure=False,
                    forbidden_tokens=(token,),
                )

        self.assertIn("exposed a bearer canary", str(raised.exception))
        self.assertNotIn(token, str(raised.exception))

    def test_quickstart_runtime_rejects_relative_installed_executable(self) -> None:
        executor = mock.Mock()
        with tempfile.TemporaryDirectory(prefix="xenoteer-runtime-") as temporary:
            with self.assertRaisesRegex(MODULE.GateError, "must be absolute"):
                MODULE.run_one_quickstart(
                    executor,
                    identity=MODULE.BuildIdentity(
                        os.geteuid(),
                        os.getegid(),
                        Path.home(),
                        False,
                    ),
                    name="rust-crate",
                    command=("relative-consumer",),
                    installed_root=Path(temporary),
                    api_base="http://127.0.0.1:43210",
                    token="token",
                    expect_auth_failure=False,
                    forbidden_tokens=("token",),
                )
        executor.run.assert_not_called()
        executor.run_as_identity.assert_not_called()

    def test_quickstart_runtime_rejects_untrusted_or_inaccessible_installed_roots(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-runtime-") as temporary:
            root = Path(temporary)
            executable = root / "installed-consumer"
            self._write_executable(executable)
            missing = root / "missing"
            regular_file = root / "regular-file"
            regular_file.write_text("not a directory", encoding="utf-8")
            world_writable = root / "world-writable"
            world_writable.mkdir()
            world_writable.chmod(0o777)
            inaccessible = root / "inaccessible"
            inaccessible.mkdir()
            inaccessible.chmod(0o600)
            identity = MODULE.BuildIdentity(
                os.geteuid(),
                os.getegid(),
                Path.home(),
                False,
            )

            for installed_root in (
                missing,
                regular_file,
                world_writable,
                inaccessible,
            ):
                with self.subTest(installed_root=installed_root):
                    executor = mock.Mock()
                    with self.assertRaisesRegex(
                        MODULE.GateError,
                        "installed root is unavailable, untrusted, or inaccessible",
                    ):
                        MODULE.run_one_quickstart(
                            executor,
                            identity=identity,
                            name="rust-crate",
                            command=(str(executable),),
                            installed_root=installed_root,
                            api_base="http://127.0.0.1:43210",
                            token="token",
                            expect_auth_failure=False,
                            forbidden_tokens=("token",),
                        )
                    executor.run.assert_not_called()
                    executor.run_as_identity.assert_not_called()

    def test_quickstart_runtime_cwd_prevents_repository_source_shadowing(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="xenoteer-runtime-") as temporary:
            root = Path(temporary)
            repository_shadow = root / "repository"
            installed_root = root / "installed"
            repository_shadow.mkdir()
            installed_root.mkdir()
            (repository_shadow / "shadow.py").write_text(
                'ORIGIN = "repository"\n',
                encoding="utf-8",
            )
            (installed_root / "shadow.py").write_text(
                'ORIGIN = "installed"\n',
                encoding="utf-8",
            )
            output = "\n".join(
                [
                    f"quickstart-ok language=rust-crate behavior={behavior}"
                    for behavior in MODULE.REQUIRED_BEHAVIORS
                ]
                + ["quickstart-ok language=rust-crate mode=success", ""]
            )
            program = (
                "import shadow, sys; "
                "sys.exit(71) if shadow.ORIGIN != 'installed' else None; "
                f"print({output!r}, end='')"
            )
            identity = MODULE.BuildIdentity(
                os.geteuid(),
                os.getegid(),
                Path.home(),
                False,
            )
            original_cwd = Path.cwd()
            try:
                os.chdir(repository_shadow)
                MODULE.run_one_quickstart(
                    MODULE.CommandExecutor(),
                    identity=identity,
                    name="rust-crate",
                    command=(sys.executable, "-c", program),
                    installed_root=installed_root,
                    api_base="http://127.0.0.1:43210",
                    token="token",
                    expect_auth_failure=False,
                    forbidden_tokens=("token",),
                )
            finally:
                os.chdir(original_cwd)

    def test_live_gate_routes_both_runs_of_all_variants_through_one_identity(
        self,
    ) -> None:
        image_id = "sha256:" + ("a1" * 32)
        identity = MODULE.BuildIdentity(12_345, 23_456, Path("/home/builder"), True)
        installed = MODULE.InstalledQuickstarts(
            ("/installed/rust",),
            Path("/installed/rust-root"),
            ("/installed/node", "/installed/example.mjs"),
            Path("/installed/npm-root"),
            ("/installed/python", "-m", "wheel"),
            Path("/installed/wheel-root"),
            ("/installed/python", "-m", "sdist"),
            Path("/installed/sdist-root"),
        )
        artifacts = MODULE.PublicArtifacts(
            Path("/artifact/protocol"),
            Path("/artifact/sdk"),
            Path("/artifact/npm"),
            Path("/artifact/wheel"),
            Path("/artifact/sdist"),
            "b2" * 32,
        )
        image = MODULE.ExactFixtureImage(image_id, image_id, artifacts.source_tree_sha256)
        toolchain = MODULE.PackageToolchain(
            Path("/installed/node"),
            Path("/installed/npm"),
            Path("/usr/bin/git"),
            (Path("/trusted/package-bin"),),
            (24, 18, 0),
        )
        executor = RecordingExecutor()
        source_executor = object()

        def inspect(
            _: object,
            arguments: list[str],
        ) -> str:
            if arguments[0] == "port":
                return "127.0.0.1:43210"
            if arguments[-1] == "{{.Image}}":
                return image_id
            if arguments[-1] == "{{.State.ExitCode}}":
                return "0"
            raise AssertionError(f"unexpected inspect: {arguments}")

        with tempfile.TemporaryDirectory(prefix="xenoteer-live-route-") as temporary:
            with (
                mock.patch.object(MODULE, "_root_owned_token_supported", return_value=True),
                mock.patch.object(MODULE, "_docker_inspect", side_effect=inspect),
                mock.patch.object(MODULE, "wait_until_ready"),
                mock.patch.object(MODULE, "run_one_quickstart") as run_one,
                mock.patch.object(MODULE, "assert_container_logs_safe"),
                mock.patch.object(
                    MODULE,
                    "current_source_tree_hash",
                    return_value=artifacts.source_tree_sha256,
                ) as source_hash,
            ):
                MODULE.run_live_gate(
                    Path("/repository"),
                    Path(temporary),
                    artifacts,
                    installed,
                    image,
                    executor,
                    identity,
                    toolchain,
                    source_executor,
                )

        self.assertEqual(run_one.call_count, 8)
        source_hash.assert_called_once_with(
            Path("/repository"),
            source_executor,
        )
        self.assertEqual(
            [call.kwargs["name"] for call in run_one.call_args_list],
            [
                "rust-crate",
                "rust-crate",
                "npm-tarball",
                "npm-tarball",
                "python-wheel",
                "python-wheel",
                "python-sdist",
                "python-sdist",
            ],
        )
        self.assertTrue(
            all(call.kwargs["identity"] == identity for call in run_one.call_args_list)
        )
        self.assertTrue(
            all(
                call.kwargs["source_environment"]
                == toolchain.source_environment()
                for call in run_one.call_args_list
            )
        )

    @staticmethod
    def _write_runtime_parity_root(
        root: Path,
        contents: dict[str, bytes],
    ) -> None:
        entries: list[tuple[str, str]] = []
        for absolute_path, payload in contents.items():
            target = root.joinpath(*Path(absolute_path).parts[1:])
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(payload)
            target.chmod(0o755 if absolute_path.startswith("/usr/local/bin/") else 0o644)
            entries.append((absolute_path, MODULE.file_sha256(target)))
        manifest = root.joinpath(*Path(MODULE.FIRST_PARTY_MANIFEST_PATH).parts[1:])
        manifest.parent.mkdir(parents=True, exist_ok=True)
        lines = ["path\tsha256\tlicense_expression\tlicense_evidence"]
        lines.extend(
            f"{path}\t{digest}\tBUSL-1.1\t/usr/share/doc/xenoteer/LICENSE"
            for path, digest in sorted(entries)
        )
        manifest.write_text("\n".join(lines) + "\n", encoding="utf-8")

    def test_rejects_every_known_daemon_override_even_when_empty(self) -> None:
        for variable in MODULE.DAEMON_OVERRIDE_ENVIRONMENTS:
            with self.subTest(variable=variable):
                with self.assertRaisesRegex(MODULE.GateError, variable):
                    MODULE.reject_daemon_overrides({variable: ""})

    def test_accepts_only_a_lowercase_immutable_docker_image_id(self) -> None:
        digest = "sha256:" + ("a5" * 32)
        self.assertEqual(MODULE.validate_image_id(digest), digest)
        for invalid in (
            "xenoteer:release-candidate",
            "sha256:" + ("A5" * 32),
            "sha256:" + ("a5" * 31),
            digest + "\nsha256:" + ("b6" * 32),
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaises(MODULE.GateError):
                    MODULE.validate_image_id(invalid)

    def test_fixture_identity_requires_exact_base_ancestry_and_source(self) -> None:
        production = "sha256:" + ("a1" * 32)
        fixture = "sha256:" + ("b2" * 32)
        source = "c3" * 32
        electron_digest = "d4" * 32
        inherited_labels = {
            "com.aeor.xenoteer.source-tree.sha256": source,
            "com.aeor.xenoteer.protocol": "v1",
        }
        inherited_config = {
            "Env": ["DISPLAY=:99", "XENOTEER__SERVER__LISTEN=0.0.0.0:8080"],
            "Entrypoint": ["/init"],
            "Cmd": None,
            "Healthcheck": {"Test": ["CMD", "/healthcheck"]},
            "User": "root",
            "WorkingDir": "",
            "StopSignal": "SIGTERM",
        }
        inspected_values = [
            {
                "Id": production,
                "RootFS": {"Layers": ["sha256:base-one", "sha256:base-two"]},
                "Config": {
                    **inherited_config,
                    "Labels": inherited_labels,
                },
            },
            {
                "Id": fixture,
                "RootFS": {
                    "Layers": [
                        "sha256:base-one",
                        "sha256:base-two",
                        "sha256:fixture-one",
                    ]
                },
                "Config": {
                    **inherited_config,
                    "Labels": {
                        **inherited_labels,
                        "com.aeor.xenoteer.distribution-scope": (
                            "test-only-non-distributable"
                        ),
                        "com.aeor.xenoteer.fixture": "phase-2-desktop-apps",
                        "com.aeor.xenoteer.fixture.debian-snapshot": (
                            MODULE.FIXTURE_DEBIAN_SNAPSHOT
                        ),
                        "com.aeor.xenoteer.fixture.base-image-id": production,
                        "com.aeor.xenoteer.fixture.electron-version": "43.1.1",
                        (
                            "com.aeor.xenoteer.fixture."
                            "electron-linux-x64-sha256"
                        ): electron_digest,
                    },
                },
            },
        ]
        inspected = json.dumps(inspected_values)
        identity = MODULE.validate_fixture_image_metadata(
            fixture,
            production,
            inspected,
        )
        self.assertEqual(identity.fixture_id, fixture)
        self.assertEqual(identity.production_id, production)
        self.assertEqual(identity.source_tree_sha256, source)
        self.assertEqual(identity.electron_version, "43.1.1")
        self.assertEqual(identity.electron_linux_x64_sha256, electron_digest)

        mutations = [
            inspected.replace(production, "sha256:" + ("d4" * 32), 1),
            inspected.replace("phase-2-desktop-apps", "wrong-fixture", 1),
            inspected.replace('"sha256:base-two", "sha256:fixture-one"', '"sha256:other"'),
            inspected.replace(source, "not-a-source-hash", 1),
        ]
        for field, value in (
            ("Env", ["XENOTEERD_BINARY_OVERRIDE=/tmp/fake"]),
            ("Entrypoint", ["/tmp/fake"]),
            ("Cmd", ["/tmp/fake"]),
            ("Healthcheck", {"Test": ["CMD", "/tmp/fake"]}),
            ("User", "nobody"),
            ("WorkingDir", "/tmp"),
            ("StopSignal", "SIGKILL"),
        ):
            mutation = copy.deepcopy(inspected_values)
            mutation[1]["Config"][field] = value
            mutations.append(json.dumps(mutation))
        for label, value in (
            ("com.aeor.xenoteer.protocol", "v2"),
            ("com.aeor.xenoteer.unexpected", "allowed"),
            ("com.aeor.xenoteer.fixture.electron-version", "unvalidated"),
        ):
            mutation = copy.deepcopy(inspected_values)
            mutation[1]["Config"]["Labels"][label] = value
            mutations.append(json.dumps(mutation))
        missing_label = copy.deepcopy(inspected_values)
        del missing_label[1]["Config"]["Labels"][
            "com.aeor.xenoteer.fixture.debian-snapshot"
        ]
        mutations.append(json.dumps(missing_label))
        missing_config = copy.deepcopy(inspected_values)
        missing_config[1]["Config"] = None
        mutations.append(json.dumps(missing_config))
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                with self.assertRaises(MODULE.GateError):
                    MODULE.validate_fixture_image_metadata(
                        fixture,
                        production,
                        mutation,
                    )

    def test_copied_runtime_parity_accepts_only_identical_manifest_backed_trees(
        self,
    ) -> None:
        contents = {
            "/usr/local/bin/xenoteerd": b"production-daemon",
            "/etc/xenoteer/config.toml": b"[server]\n",
        }
        critical_paths = ("/usr/local/bin/xenoteerd", "/etc/xenoteer")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            production = root / "production"
            fixture = root / "fixture"
            self._write_runtime_parity_root(production, contents)
            self._write_runtime_parity_root(fixture, contents)
            MODULE.validate_copied_runtime_parity(
                production,
                fixture,
                critical_paths=critical_paths,
            )

            (fixture / "usr/local/bin/xenoteerd").write_bytes(b"overwritten-daemon")
            with self.assertRaisesRegex(MODULE.GateError, "manifest hash"):
                MODULE.validate_copied_runtime_parity(
                    production,
                    fixture,
                    critical_paths=critical_paths,
                )

            self._write_runtime_parity_root(production, contents)
            self._write_runtime_parity_root(fixture, contents)
            (fixture / "etc/xenoteer/config.toml").write_bytes(
                b"[server]\noverride=true\n"
            )
            with self.assertRaisesRegex(MODULE.GateError, "manifest hash"):
                MODULE.validate_copied_runtime_parity(
                    production,
                    fixture,
                    critical_paths=critical_paths,
                )

            self._write_runtime_parity_root(production, contents)
            self._write_runtime_parity_root(fixture, contents)
            injected = fixture / "etc/xenoteer/injected.toml"
            injected.write_text("unexpected=true\n", encoding="utf-8")
            with self.assertRaisesRegex(MODULE.GateError, "critical runtime tree"):
                MODULE.validate_copied_runtime_parity(
                    production,
                    fixture,
                    critical_paths=critical_paths,
                )

            injected.unlink()
            (production / "etc/xenoteer/escape").symlink_to("../../../outside")
            (fixture / "etc/xenoteer/escape").symlink_to("../../../outside")
            with self.assertRaisesRegex(MODULE.GateError, "symlink"):
                MODULE.validate_copied_runtime_parity(
                    production,
                    fixture,
                    critical_paths=critical_paths,
                )

    def test_runtime_parity_rejects_lies_additions_and_escaping_symlinks(self) -> None:
        contents = {
            "/usr/local/bin/xenoteerd": b"production-daemon",
            "/etc/xenoteer/config.toml": b"[server]\n",
        }
        critical_paths = ("/usr/local/bin/xenoteerd", "/etc/xenoteer")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            production = root / "production"
            fixture = root / "fixture"
            self._write_runtime_parity_root(production, contents)
            self._write_runtime_parity_root(fixture, contents)
            production_manifest = production.joinpath(
                *Path(MODULE.FIRST_PARTY_MANIFEST_PATH).parts[1:]
            )
            fixture_manifest = fixture.joinpath(
                *Path(MODULE.FIRST_PARTY_MANIFEST_PATH).parts[1:]
            )

            lying = production_manifest.read_text(encoding="utf-8").replace(
                MODULE.file_sha256(production / "usr/local/bin/xenoteerd"),
                "0" * 64,
            )
            production_manifest.write_text(lying, encoding="utf-8")
            fixture_manifest.write_text(lying, encoding="utf-8")
            with self.assertRaisesRegex(MODULE.GateError, "manifest hash"):
                MODULE.validate_copied_runtime_parity(
                    production,
                    fixture,
                    critical_paths=critical_paths,
                )

    def test_fixture_image_inputs_must_byte_match_the_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture_root = root / "copied-fixture"
            repository_root = root / "repository"
            copied_sources = fixture_root.joinpath(
                *Path(MODULE.FIXTURE_IMAGE_PATH).parts[1:]
            )
            repository_sources = repository_root / MODULE.FIXTURE_REPOSITORY_PATH
            copied_lock = fixture_root.joinpath(
                *Path(MODULE.FIXTURE_ARTIFACT_LOCK_IMAGE_PATH).parts[1:]
            )
            repository_lock = (
                repository_root / MODULE.FIXTURE_ARTIFACT_LOCK_REPOSITORY_PATH
            )
            for sources in (copied_sources, repository_sources):
                sources.mkdir(parents=True)
                (sources / "gtk3-fixture.py").write_bytes(b"exact fixture\n")
                (sources / "fixture.html").write_bytes(b"<p>exact</p>\n")
            for lock in (copied_lock, repository_lock):
                lock.parent.mkdir(parents=True, exist_ok=True)
                lock.write_bytes(b"electron\tsha256:locked\n")

            MODULE.validate_fixture_repository_inputs(
                fixture_root,
                repository_root,
            )
            (copied_sources / "gtk3-fixture.py").write_bytes(b"fake fixture\n")
            with self.assertRaisesRegex(MODULE.GateError, "fixture image sources"):
                MODULE.validate_fixture_repository_inputs(
                    fixture_root,
                    repository_root,
                )

            (copied_sources / "gtk3-fixture.py").write_bytes(b"exact fixture\n")
            (copied_sources / "gtk3-fixture.py").chmod(0o755)
            with self.assertRaisesRegex(MODULE.GateError, "fixture image sources"):
                MODULE.validate_fixture_repository_inputs(
                    fixture_root,
                    repository_root,
                )

            (copied_sources / "gtk3-fixture.py").chmod(0o664)
            (copied_sources / "unexpected-empty").mkdir()
            with self.assertRaisesRegex(MODULE.GateError, "fixture image sources"):
                MODULE.validate_fixture_repository_inputs(
                    fixture_root,
                    repository_root,
                )

            (copied_sources / "unexpected-empty").rmdir()
            copied_lock.write_bytes(b"electron\tsha256:changed\n")
            with self.assertRaisesRegex(MODULE.GateError, "artifact lock"):
                MODULE.validate_fixture_repository_inputs(
                    fixture_root,
                    repository_root,
                )

    def test_runtime_evidence_uses_only_stopped_container_copying(self) -> None:
        production_id = "sha256:" + ("a1" * 32)
        fixture_id = "sha256:" + ("b2" * 32)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            image_roots = {
                production_id: root / "production-image",
                fixture_id: root / "fixture-image",
            }
            contents = {
                "/etc/s6-overlay/s6-rc.d/xenoteerd/run": b"#!/bin/sh\n",
                "/etc/xenoteer/config.toml": b"[server]\n",
                "/usr/local/libexec/xenoteer/session": b"#!/bin/sh\n",
                "/usr/share/novnc/mandatory.json": b"{}\n",
                "/usr/share/xenoteer/profile.txt": b"profile\n",
                f"{MODULE.FIXTURE_IMAGE_PATH}/gtk3-fixture.py": b"fixture\n",
                "/etc/at-spi2/accessibility.conf": b"[a11y]\n",
                "/etc/dbus-1/session-local.conf": b"<busconfig/>\n",
                "/usr/local/bin/xenoteerd": b"daemon\n",
                "/usr/local/bin/xenoteer-processd": b"processd\n",
            }
            for image_root in image_roots.values():
                self._write_runtime_parity_root(image_root, contents)
                artifact_lock = image_root.joinpath(
                    *Path(MODULE.FIXTURE_ARTIFACT_LOCK_IMAGE_PATH).parts[1:]
                )
                artifact_lock.parent.mkdir(parents=True, exist_ok=True)
                artifact_lock.write_bytes(
                    b"ELECTRON_VERSION=43.1.1\n"
                    b"ELECTRON_LINUX_X64_URL=https://example.invalid/electron.zip\n"
                    + b"ELECTRON_LINUX_X64_SHA256="
                    + (b"d4" * 32)
                    + b"\n"
                )

            repository_root = root / "repository"
            repository_fixture = (
                repository_root / MODULE.FIXTURE_REPOSITORY_PATH
            )
            repository_fixture.mkdir(parents=True)
            (repository_fixture / "gtk3-fixture.py").write_bytes(b"fixture\n")
            (repository_fixture / "gtk3-fixture.py").chmod(0o644)
            repository_lock = (
                repository_root / MODULE.FIXTURE_ARTIFACT_LOCK_REPOSITORY_PATH
            )
            repository_lock.parent.mkdir(parents=True)
            repository_lock.write_bytes(
                b"ELECTRON_VERSION=43.1.1\n"
                b"ELECTRON_LINUX_X64_URL=https://example.invalid/electron.zip\n"
                + b"ELECTRON_LINUX_X64_SHA256="
                + (b"d4" * 32)
                + b"\n"
            )
            executor = StoppedCopyExecutor(image_roots)

            MODULE.validate_fixture_runtime_parity(
                executor,
                MODULE.ExactFixtureImage(
                    fixture_id,
                    production_id,
                    "c3" * 32,
                    MODULE.FIXTURE_DEBIAN_SNAPSHOT,
                    "43.1.1",
                    "d4" * 32,
                ),
                root / "evidence",
                repository_root,
            )

            actions = [command[1] for command in executor.commands]
            self.assertEqual(actions.count("create"), 2)
            self.assertGreater(actions.count("cp"), 2)
            self.assertEqual(actions.count("inspect"), 8)
            self.assertEqual(actions.count("rm"), 2)
            self.assertFalse({"exec", "run", "start"} & set(actions))
            self.assertEqual(executor.containers, {})

    def test_source_snapshot_digest_binds_head_diff_mode_path_and_content(self) -> None:
        baseline = MODULE.source_snapshot_digest(
            "1" * 40,
            b"diff --git a/file b/file\n",
            (
                MODULE.UntrackedSource("new.txt", "644", "2" * 64),
                MODULE.UntrackedSource("script", "755", "3" * 64),
            ),
        )
        self.assertEqual(len(baseline), 64)
        mutations = (
            ("2" * 40, b"diff --git a/file b/file\n", (
                MODULE.UntrackedSource("new.txt", "644", "2" * 64),
                MODULE.UntrackedSource("script", "755", "3" * 64),
            )),
            ("1" * 40, b"changed", (
                MODULE.UntrackedSource("new.txt", "644", "2" * 64),
                MODULE.UntrackedSource("script", "755", "3" * 64),
            )),
            ("1" * 40, b"diff --git a/file b/file\n", (
                MODULE.UntrackedSource("new.txt", "600", "2" * 64),
                MODULE.UntrackedSource("script", "755", "3" * 64),
            )),
            ("1" * 40, b"diff --git a/file b/file\n", (
                MODULE.UntrackedSource("renamed.txt", "644", "2" * 64),
                MODULE.UntrackedSource("script", "755", "3" * 64),
            )),
            ("1" * 40, b"diff --git a/file b/file\n", (
                MODULE.UntrackedSource("new.txt", "644", "4" * 64),
                MODULE.UntrackedSource("script", "755", "3" * 64),
            )),
        )
        for head, diff, untracked in mutations:
            with self.subTest(head=head, diff=diff, untracked=untracked):
                self.assertNotEqual(
                    MODULE.source_snapshot_digest(head, diff, untracked),
                    baseline,
                )

    def test_dynamic_port_parser_requires_one_numeric_loopback_binding(self) -> None:
        self.assertEqual(MODULE.parse_loopback_port("127.0.0.1:49152\n"), 49152)
        for invalid in (
            "",
            "0.0.0.0:49152",
            "127.0.0.1:0",
            "127.0.0.1:65536",
            "127.0.0.1:49152\n127.0.0.1:49153",
            "[::1]:49152",
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaises(MODULE.GateError):
                    MODULE.parse_loopback_port(invalid)

    def test_npm_pack_inventory_accepts_exact_v10_and_v12_shapes(self) -> None:
        expected = "xenoteer-sdk-0.1.0.tgz"
        v10 = json.dumps([{"name": "@xenoteer/sdk", "filename": expected}])
        v12 = json.dumps(
            {
                "@xenoteer/sdk": {
                    "name": "@xenoteer/sdk",
                    "filename": expected,
                }
            }
        )
        self.assertEqual(MODULE.parse_npm_pack_filename(v10), expected)
        self.assertEqual(MODULE.parse_npm_pack_filename(v12), expected)
        for invalid in (
            "{}",
            "[]",
            json.dumps([{"name": "other", "filename": expected}]),
            json.dumps(
                {
                    "@xenoteer/sdk": {
                        "name": "@xenoteer/sdk",
                        "filename": "../escape.tgz",
                    }
                }
            ),
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaises(MODULE.GateError):
                    MODULE.parse_npm_pack_filename(invalid)

    def test_crate_extraction_accepts_only_unique_regular_prefixed_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "xenoteer-sdk-0.1.0.crate"
            with tarfile.open(archive, "w:gz") as package:
                payload = b"[package]\nname='xenoteer-sdk'\n"
                member = tarfile.TarInfo("xenoteer-sdk-0.1.0/Cargo.toml")
                member.size = len(payload)
                member.mode = 0o644
                package.addfile(member, io.BytesIO(payload))
            destination = root / "extracted"
            MODULE.extract_crate_archive(
                archive,
                destination,
                expected_prefix="xenoteer-sdk-0.1.0",
            )
            self.assertEqual(
                (destination / "Cargo.toml").read_bytes(),
                b"[package]\nname='xenoteer-sdk'\n",
            )

    def test_crate_extraction_rejects_traversal_symlinks_and_duplicate_members(self) -> None:
        cases = ("traversal", "symlink", "duplicate")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "bad.crate"
                with tarfile.open(archive, "w:gz") as package:
                    if case == "traversal":
                        member = tarfile.TarInfo("xenoteer-sdk-0.1.0/../escape")
                        member.size = 1
                        package.addfile(member, io.BytesIO(b"x"))
                    elif case == "symlink":
                        member = tarfile.TarInfo("xenoteer-sdk-0.1.0/link")
                        member.type = tarfile.SYMTYPE
                        member.linkname = "../../outside"
                        package.addfile(member)
                    else:
                        for payload in (b"a", b"b"):
                            member = tarfile.TarInfo("xenoteer-sdk-0.1.0/file")
                            member.size = 1
                            package.addfile(member, io.BytesIO(payload))
                with self.assertRaises(MODULE.GateError):
                    MODULE.extract_crate_archive(
                        archive,
                        root / "out",
                        expected_prefix="xenoteer-sdk-0.1.0",
                    )
                self.assertFalse((root / "escape").exists())

    def test_cargo_metadata_must_resolve_public_crates_from_staged_archives(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact_root = root / "artifacts"
            repository_root = root / "repository"
            metadata = {
                "packages": [
                    {
                        "name": "xenoteer-sdk",
                        "manifest_path": str(
                            artifact_root / "xenoteer-sdk" / "Cargo.toml"
                        ),
                    },
                    {
                        "name": "xenoteer-protocol",
                        "manifest_path": str(
                            artifact_root / "xenoteer-protocol" / "Cargo.toml"
                        ),
                    },
                ]
            }
            MODULE.validate_cargo_artifact_origins(
                json.dumps(metadata),
                artifact_root,
                repository_root,
            )
            metadata["packages"][0]["manifest_path"] = str(
                repository_root / "crates" / "xenoteer-sdk" / "Cargo.toml"
            )
            with self.assertRaisesRegex(MODULE.GateError, "source tree"):
                MODULE.validate_cargo_artifact_origins(
                    json.dumps(metadata),
                    artifact_root,
                    repository_root,
                )

    def test_container_guard_removes_created_container_on_success_and_failure(self) -> None:
        for raises in (False, True):
            with self.subTest(raises=raises):
                executor = RecordingExecutor()
                try:
                    with MODULE.ContainerGuard(executor, "phase6-test") as guard:
                        guard.mark_created()
                        if raises:
                            raise RuntimeError("injected gate failure")
                except RuntimeError:
                    if not raises:
                        raise
                self.assertIn(
                    (
                        "docker",
                        "rm",
                        "--force",
                        "--volumes",
                        "phase6-test",
                    ),
                    executor.commands,
                )

    def test_container_guard_does_not_hide_a_cleanup_failure(self) -> None:
        executor = RecordingExecutor(fail_cleanup=True)
        with self.assertRaisesRegex(MODULE.GateError, "cleanup"):
            with MODULE.ContainerGuard(executor, "phase6-test") as guard:
                guard.mark_created()

    def test_sigterm_handler_unwinds_through_context_cleanup(self) -> None:
        executor = RecordingExecutor()
        with self.assertRaises(KeyboardInterrupt):
            with MODULE.ContainerGuard(executor, "phase6-test") as guard:
                guard.mark_created()
                MODULE._raise_interrupted(15, None)
        self.assertIn(
            ("docker", "rm", "--force", "--volumes", "phase6-test"),
            executor.commands,
        )

    def test_quickstart_sources_are_bounded_and_verify_installed_origins(self) -> None:
        sources = {
            "rust": REPOSITORY_ROOT
            / "crates"
            / "xenoteer-sdk"
            / "examples"
            / "phase6_behaviors.rs",
            "typescript": REPOSITORY_ROOT
            / "packages"
            / "typescript"
            / "examples"
            / "phase6-behaviors.mjs",
            "python": REPOSITORY_ROOT
            / "packages"
            / "python"
            / "src"
            / "xenoteer"
            / "examples"
            / "phase6_behaviors.py",
        }
        for language, path in sources.items():
            with self.subTest(language=language):
                source = path.read_text(encoding="utf-8")
                self.assertIn("XENOTEER_EXPECTED_INSTALL_ROOT", source)
                self.assertIn("XENOTEER_EXPECT_AUTH_FAILURE", source)
                self.assertIn("XENOTEER_API_BASE", source)
                self.assertIn("XENOTEER_TOKEN", source)
                self.assertNotIn("packages/", source)
                self.assertNotIn("crates/", source)
                self.assertNotIn("sleep(", source)
                for behavior in MODULE.REQUIRED_BEHAVIORS:
                    self.assertIn(behavior, source)
                scoped_api = {
                    "rust": "with_control",
                    "typescript": "withControl",
                    "python": "desktop.control",
                }[language]
                self.assertIn(scoped_api, source)
                cleanup_aggregate = {
                    "rust": "ControlScopeError",
                    "typescript": "AggregateError",
                    "python": "BaseExceptionGroup",
                }[language]
                self.assertIn(cleanup_aggregate, source)

    def test_installed_quickstarts_never_copy_repository_quickstart_sources(self) -> None:
        source = MODULE_PATH.read_text(encoding="utf-8")
        function_start = source.index("def prepare_installed_quickstarts(")
        function = source[
            function_start : source.index("\ndef _docker_inspect(", function_start)
        ]
        self.assertNotIn('"quickstarts"', function)
        self.assertNotIn("shutil.copyfile(", function)
        self.assertIn("examples/phase6_behaviors.rs", function)
        self.assertIn("examples/phase6-behaviors.mjs", function)
        self.assertIn("xenoteer.examples.phase6_behaviors", function)

    def test_one_resolved_package_toolchain_reaches_every_node_npm_call_site(
        self,
    ) -> None:
        source = MODULE_PATH.read_text(encoding="utf-8")
        stage_start = source.index("def stage_public_artifacts(")
        prepare_start = source.index("def prepare_installed_quickstarts(")
        live_start = source.index("def run_live_gate(")
        qualify_start = source.index("def qualify(")
        stage = source[stage_start:prepare_start]
        prepare = source[prepare_start:source.index("\ndef _docker_inspect(", prepare_start)]
        live = source[live_start:qualify_start]
        qualify = source[qualify_start:]

        for name, function in (("stage", stage), ("prepare", prepare)):
            with self.subTest(function=name):
                self.assertIn("toolchain: PackageToolchain", function)
                self.assertIn("str(toolchain.npm)", function)
                self.assertNotIn('resolve_executable("npm"', function)
                self.assertNotIn('resolve_executable("node"', function)
        self.assertIn("str(toolchain.node)", prepare)
        self.assertIn("toolchain: PackageToolchain", live)
        self.assertIn("source_environment=toolchain.source_environment()", live)
        self.assertEqual(qualify.count("resolve_package_toolchain("), 1)
        self.assertIn("toolchain = resolve_package_toolchain(", qualify)
        self.assertGreaterEqual(qualify.count("toolchain,"), 3)

    def test_qualify_threads_the_same_toolchain_object_through_every_consumer(
        self,
    ) -> None:
        identity = object()
        toolchain = SimpleNamespace(git=Path("/usr/bin/git"))
        source_executor = object()
        source_tree = "a1" * 32
        image = SimpleNamespace(
            fixture_id="sha256:" + ("b2" * 32),
            production_id="sha256:" + ("c3" * 32),
            source_tree_sha256=source_tree,
        )
        artifacts = SimpleNamespace(
            source_tree_sha256=source_tree,
            digests=lambda: {"npm": "d4" * 32},
        )
        installed = object()

        with (
            mock.patch.object(MODULE, "reject_daemon_overrides"),
            mock.patch.object(MODULE.BuildIdentity, "current", return_value=identity),
            mock.patch.object(
                MODULE,
                "resolve_package_toolchain",
                return_value=toolchain,
            ) as resolve,
            mock.patch.object(
                MODULE,
                "SourceIdentityExecutor",
                return_value=source_executor,
            ) as source_executor_factory,
            mock.patch.object(MODULE, "resolve_exact_image", return_value=image),
            mock.patch.object(
                MODULE,
                "current_source_tree_hash",
                return_value=source_tree,
            ) as source_hash,
            mock.patch.object(MODULE, "chown_tree"),
            mock.patch.object(MODULE, "validate_fixture_runtime_parity"),
            mock.patch.object(
                MODULE,
                "stage_public_artifacts",
                return_value=artifacts,
            ) as stage,
            mock.patch.object(
                MODULE,
                "prepare_installed_quickstarts",
                return_value=installed,
            ) as prepare,
            mock.patch.object(MODULE, "run_live_gate") as live,
        ):
            MODULE.qualify(image.production_id)

        self.assertIs(resolve.call_args.args[0], identity)
        source_executor_factory.assert_called_once_with(
            identity,
            mock.ANY,
            toolchain.git,
        )
        self.assertEqual(source_hash.call_count, 3)
        self.assertTrue(
            all(
                call.args[1] is source_executor
                for call in source_hash.call_args_list
            )
        )
        self.assertIs(stage.call_args.args[-1], toolchain)
        self.assertIs(prepare.call_args.args[-1], toolchain)
        self.assertIs(live.call_args.args[-2], toolchain)
        self.assertIs(live.call_args.args[-1], source_executor)

    def test_installed_quickstarts_do_not_pass_environment_discarded_by_env_i(
        self,
    ) -> None:
        source = MODULE_PATH.read_text(encoding="utf-8")
        function = source[
            source.index("def prepare_installed_quickstarts(") :
            source.index("\ndef _docker_inspect(", source.index("def prepare_installed_quickstarts("))
        ]
        self.assertNotIn("CARGO_BUILD_JOBS", function)
        self.assertNotIn("CARGO_TERM_COLOR", function)
        self.assertNotIn("cargo_environment", function)
        self.assertIn('"--jobs",\n                "2",', function)

    def test_each_artifact_variant_gets_fresh_fixture_and_explicit_viewer_policy(self) -> None:
        source = MODULE_PATH.read_text(encoding="utf-8")
        function = source[
            source.index("def run_live_gate(") :
            source.index("\ndef qualify(", source.index("def run_live_gate("))
        ]
        variant_loop = function.index("for name, command, installed_root in installed.variants():")
        container_guard = function.index("with ContainerGuard(", variant_loop)
        self.assertLess(variant_loop, container_guard)
        self.assertIn('"XENOTEER__VIEWER__ENABLED=true"', function)
        self.assertIn("XENOTEER__VIEWER__ALLOWED_ORIGINS", function)
        self.assertIn("gtk3-fixture.py", function)
        self.assertIn("image.fixture_id", function)

    def test_success_output_requires_every_behavior_once_in_canonical_order(self) -> None:
        language = "python-wheel"
        output = "\n".join(
            [
                f"quickstart-ok language={language} behavior={behavior}"
                for behavior in MODULE.REQUIRED_BEHAVIORS
            ]
            + [f"quickstart-ok language={language} mode=success", ""]
        )
        MODULE.validate_quickstart_output(
            output,
            language=language,
            expect_auth_failure=False,
        )
        mutations = (
            output.replace(
                f"quickstart-ok language={language} behavior={MODULE.REQUIRED_BEHAVIORS[3]}\n",
                "",
                1,
            ),
            output.replace(
                f"quickstart-ok language={language} behavior={MODULE.REQUIRED_BEHAVIORS[3]}",
                f"quickstart-ok language={language} behavior={MODULE.REQUIRED_BEHAVIORS[2]}",
                1,
            ),
            "\n".join(
                output.splitlines()[1:2] + output.splitlines()[0:1] + output.splitlines()[2:]
            )
            + "\n",
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                with self.assertRaises(MODULE.GateError):
                    MODULE.validate_quickstart_output(
                        mutation,
                        language=language,
                        expect_auth_failure=False,
                    )

    def test_auth_failure_output_cannot_impersonate_behavior_execution(self) -> None:
        output = "quickstart-ok language=rust-crate mode=auth-failure\n"
        MODULE.validate_quickstart_output(
            output,
            language="rust-crate",
            expect_auth_failure=True,
        )
        with self.assertRaises(MODULE.GateError):
            MODULE.validate_quickstart_output(
                output
                + "quickstart-ok language=rust-crate "
                + f"behavior={MODULE.REQUIRED_BEHAVIORS[0]}\n",
                language="rust-crate",
                expect_auth_failure=True,
            )

    def test_package_examples_keep_transport_deadline_above_server_long_polls(
        self,
    ) -> None:
        validate_package_example_deadlines()

    def _assert_deadline_source_mutation_rejected(
        self,
        path: Path,
        mutate: Callable[[str], str],
    ) -> None:
        original_read_text = Path.read_text

        def mutated_read_text(
            candidate: Path,
            *args: object,
            **kwargs: object,
        ) -> str:
            source = original_read_text(candidate, *args, **kwargs)
            if candidate == path:
                return mutate(source)
            return source

        with mock.patch.object(Path, "read_text", mutated_read_text):
            with self.assertRaises(AssertionError):
                validate_package_example_deadlines()

    def test_deadline_contract_rejects_every_unconfigured_transport_constructor(
        self,
    ) -> None:
        additions = {
            "rust": (
                "\nfn review_bypass(base: &str, token: &str) {\n"
                "    let _ = Client::new(base, token.as_bytes());\n}\n"
            ),
            "typescript": (
                "\nasync function reviewBypass(baseUrl, token) {\n"
                "  return await XenoteerClient.connect({ baseUrl, token });\n}\n"
            ),
            "python": (
                "\nasync def review_bypass(base_url: str, token: str) -> None:\n"
                "    options = ClientOptions(base_url=base_url, token=token)\n"
                "    await XenoteerClient.connect(options)\n"
            ),
        }
        for language, addition in additions.items():
            with self.subTest(language=language):
                self._assert_deadline_source_mutation_rejected(
                    EXAMPLE_PATHS[language],
                    lambda source, addition=addition: source + addition,
                )

    def test_deadline_contract_rejects_aliased_client_constructors(self) -> None:
        additions = {
            "rust-import": (
                "rust",
                "\nuse reqwest::Client as ReviewClient;\n"
                "fn review(base: &str, token: &str) {\n"
                "    let _ = ReviewClient::new(base, token.as_bytes());\n}\n",
            ),
            "rust-type": (
                "rust",
                "\ntype ReviewClient = Client;\n"
                "fn review(base: &str, token: &str) {\n"
                "    let _ = ReviewClient::new(base, token.as_bytes());\n}\n",
            ),
            "rust-local": (
                "rust",
                "\nfn review(base: &str, token: &str) {\n"
                "    use reqwest::Client as ReviewClient;\n"
                "    let _ = ReviewClient::new(base, token.as_bytes());\n}\n",
            ),
            "typescript": (
                "typescript",
                "\nconst ReviewClient = XenoteerClient;\n"
                "async function review(options) {\n"
                "  return await ReviewClient.connect(options);\n}\n",
            ),
            "python": (
                "python",
                "\nReviewClient = XenoteerClient\n"
                "async def review(options: Any) -> None:\n"
                "    await ReviewClient.connect(options)\n",
            ),
        }
        for mutation, (language, addition) in additions.items():
            with self.subTest(mutation=mutation):
                self._assert_deadline_source_mutation_rejected(
                    EXAMPLE_PATHS[language],
                    lambda source, addition=addition: source + addition,
                )

    def test_deadline_contract_rejects_extracted_wait_members(self) -> None:
        additions = {
            ("rust", "windows"): (
                "\nasync fn review(desktop: &Desktop, request: &WindowWaitRequest) {\n"
                "    let windows = desktop.windows();\n"
                "    let _ = windows.wait(request).await;\n}\n"
            ),
            ("rust", "accessibility"): (
                "\nasync fn review(desktop: &Desktop, request: &ElementWaitRequest) {\n"
                "    let accessibility = desktop.accessibility();\n"
                "    let _ = accessibility.wait(request).await;\n}\n"
            ),
            ("typescript", "windows"): (
                "\nasync function review(desktop, request) {\n"
                "  const windows = desktop.windows;\n"
                "  await windows.wait(request);\n}\n"
            ),
            ("typescript", "accessibility"): (
                "\nasync function review(desktop, request) {\n"
                "  const accessibility = desktop.accessibility;\n"
                "  await accessibility.wait(request);\n}\n"
            ),
            ("python", "windows"): (
                "\nasync def review(desktop: Any, request: Any) -> None:\n"
                "    windows = desktop.windows\n"
                "    await windows.wait(request)\n"
            ),
            ("python", "accessibility"): (
                "\nasync def review(desktop: Any, request: Any) -> None:\n"
                "    accessibility = desktop.accessibility\n"
                "    await accessibility.wait(request)\n"
            ),
        }
        for (language, domain), addition in additions.items():
            with self.subTest(language=language, domain=domain):
                self._assert_deadline_source_mutation_rejected(
                    EXAMPLE_PATHS[language],
                    lambda source, addition=addition: source + addition,
                )

    def test_deadline_contract_rejects_unverifiable_server_waits(self) -> None:
        additions = {
            "omitted": (
                "\nasync function reviewWait(desktop) {\n"
                "  await desktop.windows.wait({ predicate: { type: 'exists' } });\n}\n"
            ),
            "aliased": (
                "\nasync function reviewWait(desktop, deadline) {\n"
                "  await desktop.windows.wait({ timeout_ms: deadline });\n}\n"
            ),
            "computed": (
                "\nasync function reviewWait(desktop) {\n"
                "  await desktop.windows.wait({\n"
                "    timeout_ms: SERVER_LONG_POLL_TIMEOUT_MILLISECONDS - 1,\n"
                "  });\n}\n"
            ),
        }
        for mutation, addition in additions.items():
            with self.subTest(mutation=mutation):
                self._assert_deadline_source_mutation_rejected(
                    EXAMPLE_PATHS["typescript"],
                    lambda source, addition=addition: source + addition,
                )

    def test_deadline_contract_rejects_lowered_internal_deadlines(self) -> None:
        replacements = {
            "rust": (
                "Duration::from_millis(EXAMPLE_OVERALL_TIMEOUT_MILLISECONDS)",
                "Duration::from_millis(20_000)",
            ),
            "python": (
                "timeout=EXAMPLE_OVERALL_TIMEOUT_MILLISECONDS / 1_000",
                "timeout=20",
            ),
        }
        for language, (original, lowered) in replacements.items():
            with self.subTest(language=language):
                self._assert_deadline_source_mutation_rejected(
                    EXAMPLE_PATHS[language],
                    lambda source, original=original, lowered=lowered: source.replace(
                        original,
                        lowered,
                        1,
                    ),
                )

    def test_deadline_contract_rejects_lowered_external_deadline(self) -> None:
        with mock.patch.object(MODULE, "QUICKSTART_COMMAND_TIMEOUT_SECONDS", 20):
            with self.assertRaises(AssertionError):
                self.test_package_examples_keep_transport_deadline_above_server_long_polls()

    def test_every_external_subprocess_timeout_is_bounded(self) -> None:
        self.assertLessEqual(MODULE.DEFAULT_COMMAND_TIMEOUT_SECONDS, 10)
        self.assertLessEqual(MODULE.PACKAGE_COMMAND_TIMEOUT_SECONDS, 120)
        self.assertLessEqual(MODULE.QUICKSTART_COMMAND_TIMEOUT_SECONDS, 120)


if __name__ == "__main__":
    unittest.main()
