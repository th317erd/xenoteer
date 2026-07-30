#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Focused contracts for the side-effect-free release identity boundary."""

from __future__ import annotations

import copy
import importlib.util
import json
import os
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
MODULE_PATH = REPOSITORY_ROOT / "scripts" / "sdk" / "qualification_identity.py"
SPEC = importlib.util.spec_from_file_location("qualification_identity", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load release identity module")
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

PRODUCTION_ID = "sha256:" + ("a1" * 32)
FIXTURE_ID = "sha256:" + ("b2" * 32)
HEAD = "c3" * 20
SOURCE_TREE = "d4" * 32
DEPENDENCY_LOCK = "e5" * 32
ELECTRON_SHA256 = "f6" * 32


class Result:
    def __init__(self, stdout: str) -> None:
        self.stdout = stdout


class SourceExecutor:
    def __init__(self, root: Path, *, dirty: bool) -> None:
        self.root = root
        self.dirty = dirty
        self.commands: list[tuple[str, ...]] = []

    def run(
        self,
        command: list[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> Result:
        del timeout
        self.commands.append(tuple(command))
        if cwd != self.root:
            raise AssertionError(f"unexpected cwd: {cwd}")
        if command == ["git", "rev-parse", "--verify", "HEAD"]:
            return Result(HEAD + "\n")
        if command == [
            "git",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ]:
            return Result(" M tracked\n" if self.dirty else "")
        raise AssertionError(f"unexpected text command: {command!r}")

    def run_bytes(
        self,
        command: list[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> bytes:
        del timeout
        self.commands.append(tuple(command))
        if cwd != self.root:
            raise AssertionError(f"unexpected cwd: {cwd}")
        if command == ["git", "diff", "--binary", "--no-ext-diff", "HEAD", "--"]:
            return b"diff bytes\n"
        if command == [
            "git",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ]:
            return b"untracked.txt\0"
        raise AssertionError(f"unexpected binary command: {command!r}")


def artifact_lock_text() -> str:
    return (
        "ELECTRON_VERSION=43.1.1\n"
        "ELECTRON_LINUX_X64_URL="
        "https://github.com/electron/electron/releases/download/v43.1.1/"
        "electron-v43.1.1-linux-x64.zip\n"
        f"ELECTRON_LINUX_X64_SHA256={ELECTRON_SHA256}\n"
    )


def inspected_metadata() -> list[dict[str, object]]:
    production_labels = {
        "org.opencontainers.image.revision": HEAD,
        "com.aeor.xenoteer.source.dirty": "false",
        "com.aeor.xenoteer.source-tree.sha256": SOURCE_TREE,
        "com.aeor.xenoteer.dependency-lock.sha256": DEPENDENCY_LOCK,
        "com.aeor.xenoteer.protocol": "v1",
    }
    runtime = {
        "Env": ["DISPLAY=:99"],
        "Entrypoint": ["/init"],
        "Cmd": None,
        "Healthcheck": {"Test": ["CMD", "/healthcheck"]},
        "User": "root",
        "WorkingDir": "",
        "StopSignal": "SIGTERM",
    }
    return [
        {
            "Id": PRODUCTION_ID,
            "RootFS": {"Layers": ["sha256:base-one", "sha256:base-two"]},
            "Config": {**runtime, "Labels": production_labels},
        },
        {
            "Id": FIXTURE_ID,
            "RootFS": {
                "Layers": [
                    "sha256:base-one",
                    "sha256:base-two",
                    "sha256:fixture",
                ]
            },
            "Config": {
                **runtime,
                "Labels": {
                    **production_labels,
                    "com.aeor.xenoteer.distribution-scope": (
                        "test-only-non-distributable"
                    ),
                    "com.aeor.xenoteer.fixture": "phase-2-desktop-apps",
                    "com.aeor.xenoteer.fixture.debian-snapshot": (
                        MODULE.FIXTURE_DEBIAN_SNAPSHOT
                    ),
                    "com.aeor.xenoteer.fixture.base-image-id": PRODUCTION_ID,
                    "com.aeor.xenoteer.fixture.electron-version": "43.1.1",
                    (
                        "com.aeor.xenoteer.fixture."
                        "electron-linux-x64-sha256"
                    ): ELECTRON_SHA256,
                },
            },
        },
    ]


class QualificationIdentityTests(unittest.TestCase):
    def test_import_has_no_process_signal_tempdir_or_ownership_side_effects(
        self,
    ) -> None:
        side_effects = (
            mock.patch.object(subprocess, "run"),
            mock.patch.object(signal, "signal"),
            mock.patch.object(tempfile, "TemporaryDirectory"),
            mock.patch.object(os, "chown"),
        )
        loaded_name = "qualification_identity_side_effect_probe"
        loaded_spec = importlib.util.spec_from_file_location(
            loaded_name,
            MODULE_PATH,
        )
        assert loaded_spec is not None and loaded_spec.loader is not None
        loaded_module = importlib.util.module_from_spec(loaded_spec)
        sys.modules[loaded_name] = loaded_module
        try:
            with side_effects[0] as run:
                with side_effects[1] as install_signal:
                    with side_effects[2] as temporary_directory:
                        with side_effects[3] as chown:
                            loaded_spec.loader.exec_module(loaded_module)
            run.assert_not_called()
            install_signal.assert_not_called()
            temporary_directory.assert_not_called()
            chown.assert_not_called()
        finally:
            sys.modules.pop(loaded_name, None)

    def test_exact_image_pair_requires_distinct_lowercase_ids(self) -> None:
        self.assertEqual(
            MODULE.validate_exact_image_ids(PRODUCTION_ID, FIXTURE_ID),
            (PRODUCTION_ID, FIXTURE_ID),
        )
        invalid_pairs = (
            ("production:candidate", FIXTURE_ID),
            (PRODUCTION_ID.upper(), FIXTURE_ID),
            ("sha256:1234", FIXTURE_ID),
            (PRODUCTION_ID + "\n", FIXTURE_ID),
            (PRODUCTION_ID, PRODUCTION_ID),
        )
        for production_id, fixture_id in invalid_pairs:
            with self.subTest(
                production_id=production_id,
                fixture_id=fixture_id,
            ):
                with self.assertRaises(MODULE.GateError):
                    MODULE.validate_exact_image_ids(production_id, fixture_id)

    def test_current_source_identity_matches_build_wrapper_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            untracked = root / "untracked.txt"
            untracked.write_bytes(b"untracked source\n")
            untracked.chmod(0o640)
            executor = SourceExecutor(root, dirty=True)
            identity = MODULE.current_source_identity(root, executor)
            expected = MODULE.source_snapshot_digest(
                HEAD,
                b"diff bytes\n",
                (
                    MODULE.UntrackedSource(
                        "untracked.txt",
                        format(stat.S_IMODE(untracked.stat().st_mode), "o"),
                        MODULE.file_sha256(untracked),
                    ),
                ),
            )
        self.assertEqual(identity.head_revision, HEAD)
        self.assertEqual(identity.source_tree_sha256, expected)
        self.assertTrue(identity.dirty)
        self.assertEqual(identity.revision, f"{HEAD}-dirty.{expected[:12]}")

    def test_dependency_digest_accepts_only_one_lowercase_sha256(self) -> None:
        self.assertEqual(
            MODULE.validate_dependency_lock_digest(DEPENDENCY_LOCK + "\n"),
            DEPENDENCY_LOCK,
        )
        for invalid in (
            DEPENDENCY_LOCK.upper(),
            DEPENDENCY_LOCK + "\nextra\n",
            DEPENDENCY_LOCK + " ",
            "sha256:" + DEPENDENCY_LOCK,
            "",
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaises(MODULE.GateError):
                    MODULE.validate_dependency_lock_digest(invalid)

    def test_artifact_lock_parser_is_exact_and_typed(self) -> None:
        lock = MODULE.parse_fixture_artifact_lock(artifact_lock_text())
        self.assertEqual(lock.electron_version, "43.1.1")
        self.assertEqual(lock.electron_linux_x64_sha256, ELECTRON_SHA256)
        for mutation in (
            artifact_lock_text() + "UNKNOWN=value\n",
            artifact_lock_text().replace(
                "ELECTRON_VERSION=43.1.1\n",
                "ELECTRON_VERSION=43.1.1\nELECTRON_VERSION=43.1.1\n",
            ),
            artifact_lock_text().replace(ELECTRON_SHA256, ELECTRON_SHA256.upper()),
            artifact_lock_text().replace("https://", "http://"),
        ):
            with self.subTest(mutation=mutation):
                with self.assertRaises(MODULE.GateError):
                    MODULE.parse_fixture_artifact_lock(mutation)

    def test_combined_release_validation_binds_all_identity_inputs(self) -> None:
        image = MODULE.validate_release_image_metadata(
            PRODUCTION_ID,
            FIXTURE_ID,
            json.dumps(inspected_metadata()),
            MODULE.SourceIdentity(HEAD, SOURCE_TREE, False, HEAD),
            DEPENDENCY_LOCK + "\n",
            MODULE.parse_fixture_artifact_lock(artifact_lock_text()),
        )
        self.assertEqual(image.production_id, PRODUCTION_ID)
        self.assertEqual(image.fixture_id, FIXTURE_ID)
        self.assertEqual(image.source_tree_sha256, SOURCE_TREE)
        self.assertEqual(image.dependency_lock_sha256, DEPENDENCY_LOCK)

    def test_combined_release_validation_rejects_each_identity_mismatch(self) -> None:
        cases: list[tuple[str, list[dict[str, object]], object]] = []
        for label, replacement in (
            ("org.opencontainers.image.revision", "0" * 40),
            ("com.aeor.xenoteer.source.dirty", "true"),
            ("com.aeor.xenoteer.source-tree.sha256", "1" * 64),
            ("com.aeor.xenoteer.dependency-lock.sha256", "2" * 64),
        ):
            values = copy.deepcopy(inspected_metadata())
            labels = values[0]["Config"]["Labels"]  # type: ignore[index]
            labels[label] = replacement  # type: ignore[index]
            cases.append(
                (
                    label,
                    values,
                    MODULE.SourceIdentity(HEAD, SOURCE_TREE, False, HEAD),
                )
            )
        dirty_source = MODULE.SourceIdentity(
            HEAD,
            SOURCE_TREE,
            True,
            f"{HEAD}-dirty.{SOURCE_TREE[:12]}",
        )
        cases.append(("dirty checkout", inspected_metadata(), dirty_source))
        wrong_base = inspected_metadata()
        fixture_labels = wrong_base[1]["Config"]["Labels"]  # type: ignore[index]
        fixture_labels[  # type: ignore[index]
            "com.aeor.xenoteer.fixture.base-image-id"
        ] = "sha256:" + ("77" * 32)
        cases.append(
            (
                "base image",
                wrong_base,
                MODULE.SourceIdentity(HEAD, SOURCE_TREE, False, HEAD),
            )
        )
        wrong_layers = inspected_metadata()
        wrong_layers[1]["RootFS"]["Layers"][0] = (  # type: ignore[index]
            "sha256:different"
        )
        cases.append(
            (
                "layer prefix",
                wrong_layers,
                MODULE.SourceIdentity(HEAD, SOURCE_TREE, False, HEAD),
            )
        )
        wrong_electron = inspected_metadata()
        electron_labels = wrong_electron[1]["Config"]["Labels"]  # type: ignore[index]
        electron_labels[  # type: ignore[index]
            "com.aeor.xenoteer.fixture.electron-linux-x64-sha256"
        ] = "3" * 64
        cases.append(
            (
                "Electron artifact lock",
                wrong_electron,
                MODULE.SourceIdentity(HEAD, SOURCE_TREE, False, HEAD),
            )
        )
        for name, values, source in cases:
            with self.subTest(name=name):
                with self.assertRaises(MODULE.GateError):
                    MODULE.validate_release_image_metadata(
                        PRODUCTION_ID,
                        FIXTURE_ID,
                        json.dumps(values),
                        source,
                        DEPENDENCY_LOCK,
                        MODULE.parse_fixture_artifact_lock(artifact_lock_text()),
                    )

    def test_fixture_metadata_api_remains_fixture_first(self) -> None:
        image = MODULE.validate_fixture_image_metadata(
            FIXTURE_ID,
            PRODUCTION_ID,
            json.dumps(inspected_metadata()),
        )
        self.assertEqual(
            (image.fixture_id, image.production_id),
            (FIXTURE_ID, PRODUCTION_ID),
        )


if __name__ == "__main__":
    unittest.main()
