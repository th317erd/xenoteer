#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Fail-closed unit contracts for the staged public quick-start gate."""

from __future__ import annotations

import importlib.util
import io
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
MODULE_PATH = REPOSITORY_ROOT / "scripts" / "sdk" / "public_quickstarts.py"
SPEC = importlib.util.spec_from_file_location("public_quickstarts", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load public quick-start gate module")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


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


class PublicQuickstartContractTests(unittest.TestCase):
    """Prove identity, artifact, auth-gate, and cleanup primitives."""

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
            / "scripts"
            / "sdk"
            / "quickstarts"
            / "rust"
            / "main.rs",
            "typescript": REPOSITORY_ROOT
            / "scripts"
            / "sdk"
            / "quickstarts"
            / "typescript"
            / "quickstart.mjs",
            "python": REPOSITORY_ROOT
            / "scripts"
            / "sdk"
            / "quickstarts"
            / "python"
            / "quickstart.py",
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

    def test_every_external_subprocess_timeout_is_bounded(self) -> None:
        self.assertLessEqual(MODULE.DEFAULT_COMMAND_TIMEOUT_SECONDS, 10)
        self.assertLessEqual(MODULE.PACKAGE_COMMAND_TIMEOUT_SECONDS, 120)
        self.assertLessEqual(MODULE.QUICKSTART_COMMAND_TIMEOUT_SECONDS, 10)


if __name__ == "__main__":
    unittest.main()
