#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Network-free contracts for the canonical Phase 6 qualification runner."""

from __future__ import annotations

import fcntl
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
from types import SimpleNamespace
import unittest
from unittest import mock


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
MODULE_PATH = REPOSITORY_ROOT / "scripts/container/qualify-phase6.py"
SPEC = importlib.util.spec_from_file_location(
    "xenoteer_phase6_qualification",
    MODULE_PATH,
)
assert SPEC is not None
if MODULE_PATH.is_file():
    assert SPEC.loader is not None
    MODULE = importlib.util.module_from_spec(SPEC)
    sys.modules[SPEC.name] = MODULE
    SPEC.loader.exec_module(MODULE)
else:
    MODULE = None


PRODUCTION_ID = "sha256:" + ("1a" * 32)
FIXTURE_ID = "sha256:" + ("2b" * 32)
SOURCE_TREE = "3c" * 32
DEPENDENCY_LOCK = "4d" * 32
REVISION = "5e" * 20
PACKAGE_DIGEST = "sha256:" + ("6f" * 32)
PACKAGE_TOOL_PATH = None if MODULE is None else MODULE._package_tool_path


def require_module() -> object:
    if MODULE is None:
        raise AssertionError(
            "missing fail-first implementation: scripts/container/qualify-phase6.py"
        )
    return MODULE


def image_metadata(
    image_id: str,
    *,
    fixture: bool,
    base_id: str = PRODUCTION_ID,
    source_tree: str = SOURCE_TREE,
    dependency_lock: str = DEPENDENCY_LOCK,
    revision: str = REVISION,
    dirty: str = "false",
    layers: tuple[str, ...] | None = None,
) -> object:
    module = require_module()
    labels = {
        "org.opencontainers.image.revision": revision,
        "com.aeor.xenoteer.source.dirty": dirty,
        "com.aeor.xenoteer.source-tree.sha256": source_tree,
        "com.aeor.xenoteer.dependency-lock.sha256": dependency_lock,
    }
    if fixture:
        labels.update(
            {
                "com.aeor.xenoteer.distribution-scope": (
                    "test-only-non-distributable"
                ),
                "com.aeor.xenoteer.fixture": "phase-2-desktop-apps",
                "com.aeor.xenoteer.fixture.debian-snapshot": "20260719T000000Z",
                "com.aeor.xenoteer.fixture.base-image-id": base_id,
                "com.aeor.xenoteer.fixture.electron-version": "43.1.1",
                (
                    "com.aeor.xenoteer.fixture."
                    "electron-linux-x64-sha256"
                ): "c1f479c52747caf1510e17500e1c8a556d0e40802837bd48c5647a84688a3880",
            }
        )
    default_layers = ("layer-a", "layer-b", "fixture-layer") if fixture else (
        "layer-a",
        "layer-b",
    )
    return module.ImageMetadata(
        image_id=image_id,
        labels=labels,
        layers=default_layers if layers is None else layers,
    )


def source_identity(
    *,
    revision: str = REVISION,
    source_tree: str = SOURCE_TREE,
    dependency_lock: str = DEPENDENCY_LOCK,
    clean: bool = True,
) -> object:
    module = require_module()
    return module.SourceIdentity(
        revision=revision,
        source_tree_sha256=source_tree,
        dependency_lock_sha256=dependency_lock,
        clean=clean,
    )


def valid_inspector(
    production_id: str,
    fixture_id: str,
) -> tuple[object, object]:
    if production_id != PRODUCTION_ID or fixture_id != FIXTURE_ID:
        raise AssertionError(
            f"unexpected image IDs: {production_id!r}, {fixture_id!r}"
        )
    return (
        image_metadata(PRODUCTION_ID, fixture=False),
        image_metadata(FIXTURE_ID, fixture=True),
    )


def expected_quickstart_summary(
    *,
    production: str = PRODUCTION_ID,
    fixture: str = FIXTURE_ID,
    source_tree: str = SOURCE_TREE,
    omit: str | None = None,
) -> str:
    fields = {
        "fixture_image": fixture,
        "npm": PACKAGE_DIGEST,
        "production_image": production,
        "python_sdist": PACKAGE_DIGEST,
        "python_wheel": PACKAGE_DIGEST,
        "rust_protocol": PACKAGE_DIGEST,
        "rust_sdk": PACKAGE_DIGEST,
        "source_tree": source_tree,
    }
    if omit is not None:
        fields.pop(omit)
    encoded = " ".join(f"{key}={fields[key]}" for key in sorted(fields))
    return f"public quick-start qualification passed: {encoded}\n"


class PackageToolPathContractTests(unittest.TestCase):
    def setUp(self) -> None:
        if PACKAGE_TOOL_PATH is None:
            raise AssertionError("Phase 6 package-tool path implementation is missing")
        self.package_tool_path = PACKAGE_TOOL_PATH
        self.temporary = tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-node-path-test-"
        )
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.home = self.root / "invoking-home"
        self.home.mkdir(mode=0o775)
        self.system_directory = self.root / "trusted-system"
        self.system_directory.mkdir(mode=0o755)
        self.uid = os.getuid()
        self.gid = os.getgid()
        self.account = SimpleNamespace(
            pw_uid=self.uid,
            pw_gid=self.gid,
            pw_dir=str(self.home),
            pw_name="phase6-builder",
        )
        self.runtime_outputs: dict[Path, object] = {}
        self.probe_commands: list[tuple[str, ...]] = []

    def install_nvm_version(
        self,
        version: str,
        *,
        include_node: bool = True,
        include_npm: bool = True,
    ) -> Path:
        version_directory = self.home / ".nvm/versions/node" / version
        bin_directory = version_directory / "bin"
        bin_directory.mkdir(parents=True, mode=0o755)
        for directory in (
            self.home / ".nvm",
            self.home / ".nvm/versions",
            self.home / ".nvm/versions/node",
            version_directory,
        ):
            directory.chmod(0o775)
        if include_node:
            node = bin_directory / "node"
            node.write_text(
                f"#!/bin/sh\nprintf '%s\\n' {version}\n",
                encoding="utf-8",
            )
            node.chmod(0o755)
            self.runtime_outputs[node] = version
        if include_npm:
            npm_target = (
                version_directory
                / "lib/node_modules/npm/bin/npm-cli.js"
            )
            npm_target.parent.mkdir(parents=True, mode=0o755)
            npm_target.write_text("#!/usr/bin/env node\n", encoding="utf-8")
            npm_target.chmod(0o775)
            (bin_directory / "npm").symlink_to(
                os.path.relpath(npm_target, bin_directory)
            )
        return version_directory

    def _runtime_probe(self, command: object, **kwargs: object) -> object:
        del kwargs
        arguments = tuple(str(argument) for argument in command)
        self.probe_commands.append(arguments)
        node_candidates = [
            Path(argument)
            for argument in arguments
            if Path(argument).is_absolute() and Path(argument).name == "node"
        ]
        if len(node_candidates) != 1:
            raise AssertionError(f"unexpected Node probe command: {arguments!r}")
        result = self.runtime_outputs[node_candidates[0]]
        if isinstance(result, BaseException):
            raise result
        if isinstance(result, tuple):
            returncode, stdout, stderr = result
        else:
            returncode, stdout, stderr = 0, f"{result}\n", ""
        return SimpleNamespace(
            returncode=returncode,
            stdout=stdout,
            stderr=stderr,
        )

    def select_path(
        self,
        *,
        as_root: bool = False,
        environment: dict[str, str] | None = None,
        primary_gid: int | None = None,
        trusted_system_path: str | None = None,
    ) -> str:
        account = SimpleNamespace(
            **{
                **vars(self.account),
                "pw_gid": self.gid if primary_gid is None else primary_gid,
            }
        )

        def lookup(uid: int) -> object:
            if uid != self.uid:
                raise KeyError(uid)
            return account

        source_environment = {
            "HOME": "/tmp/hostile-home",
            "NVM_BIN": "/tmp/hostile-nvm/bin",
            "NVM_DIR": "/tmp/hostile-nvm",
            "PATH": "/tmp/hostile-bin",
        }
        if as_root:
            source_environment.update(
                {
                    "SUDO_UID": str(self.uid),
                    "SUDO_GID": str(account.pw_gid),
                }
            )
        if environment is not None:
            source_environment.update(environment)
        with (
            mock.patch.object(
                self.package_tool_path.__globals__["os"],
                "geteuid",
                return_value=0 if as_root else self.uid,
            ),
            mock.patch.object(
                self.package_tool_path.__globals__["pwd"],
                "getpwuid",
                side_effect=lookup,
            ),
            mock.patch.object(
                self.package_tool_path.__globals__["subprocess"],
                "run",
                side_effect=self._runtime_probe,
            ),
            mock.patch.dict(
                self.package_tool_path.__globals__,
                {
                    "TRUSTED_SYSTEM_PATH": (
                        str(self.system_directory)
                        if trusted_system_path is None
                        else trusted_system_path
                    )
                },
            ),
        ):
            return self.package_tool_path(self.root, source_environment)

    def test_exact_root_lane_path_admits_node24_and_survives_sanitization(
        self,
    ) -> None:
        version_directory = self.install_nvm_version("v24.18.0")

        selected = self.select_path(as_root=True)

        expected = os.pathsep.join(
            (
                str(version_directory / "bin"),
                str(self.home / ".cargo/bin"),
                str(self.system_directory),
            )
        )
        self.assertEqual(selected, expected)
        self.assertNotIn("/tmp/hostile", selected)
        self.assertEqual(self.probe_commands, [])
        pair = require_module().ExactImagePair(
            production=image_metadata(PRODUCTION_ID, fixture=False),
            fixture=image_metadata(FIXTURE_ID, fixture=True),
            source=source_identity(),
        )
        lane = require_module().qualification_lanes(
            REPOSITORY_ROOT,
            sys.executable,
            pair,
            package_tool_path=selected,
        )[-1]
        self.assertEqual(
            lane.environment,
            (("XENOTEER_PACKAGE_BUILD_PATH", expected),),
        )

    def test_node22_only_and_node24_only_are_admitted(self) -> None:
        for version in ("v22.19.1", "v24.18.0"):
            with self.subTest(version=version):
                self.probe_commands.clear()
                version_directory = self.install_nvm_version(version)
                selected = self.select_path()
                self.assertEqual(
                    selected.split(os.pathsep)[0],
                    str(version_directory / "bin"),
                )

    def test_highest_supported_semantic_version_is_deterministic(self) -> None:
        self.install_nvm_version("v22.99.1")
        self.install_nvm_version("v24.2.0")
        expected = self.install_nvm_version("v24.18.0")

        selected = self.select_path()

        self.assertEqual(selected.split(os.pathsep)[0], str(expected / "bin"))
        self.assertEqual(self.probe_commands, [])

    def test_unsupported_versions_and_aliases_are_ignored(self) -> None:
        self.install_nvm_version("v20.20.0")
        self.install_nvm_version("v23.11.1")
        default_alias = self.home / ".nvm/versions/node/default"
        default_alias.symlink_to("v23.11.1", target_is_directory=True)

        with self.assertRaisesRegex(
            require_module().QualificationError,
            "supported Node 22 or 24",
        ):
            self.select_path()

        self.assertEqual(self.probe_commands, [])

    def test_supported_system_node_and_npm_must_share_one_fixed_directory(
        self,
    ) -> None:
        node_only = self.root / "node-only-system"
        npm_only = self.root / "npm-only-system"
        node_only.mkdir()
        npm_only.mkdir()
        node = node_only / "node"
        node.write_text(
            "#!/bin/sh\nprintf '%s\\n' v22.20.0\n",
            encoding="utf-8",
        )
        node.chmod(0o755)
        npm = npm_only / "npm"
        npm.write_text("#!/usr/bin/env node\n", encoding="utf-8")
        npm.chmod(0o755)
        self.runtime_outputs[node] = "v22.20.0"
        system_path = os.pathsep.join((str(node_only), str(npm_only)))
        module = require_module()
        original_trust = module._trusted_non_symlink_directory

        def trust_test_system(
            identity: object,
            directory: Path,
            *,
            root_owned: bool = False,
        ) -> bool:
            if root_owned and directory in {node_only, npm_only}:
                return True
            return original_trust(
                identity,
                directory,
                root_owned=root_owned,
            )

        with (
            mock.patch.object(
                module,
                "_trusted_non_symlink_directory",
                side_effect=trust_test_system,
            ),
            self.assertRaisesRegex(
                module.QualificationError,
                "supported Node 22 or 24",
            ),
        ):
            self.select_path(trusted_system_path=system_path)
        self.assertEqual(self.probe_commands, [])

    def test_coherent_supported_system_pair_is_a_safe_fallback(self) -> None:
        node = self.system_directory / "node"
        node.write_text(
            "#!/bin/sh\nprintf '%s\\n' v22.20.0\n",
            encoding="utf-8",
        )
        node.chmod(0o755)
        npm = self.system_directory / "npm"
        npm.write_text("#!/usr/bin/env node\n", encoding="utf-8")
        npm.chmod(0o755)
        self.runtime_outputs[node] = "v22.20.0"
        module = require_module()
        original_trust = module._trusted_non_symlink_directory

        def trust_test_system(
            identity: object,
            directory: Path,
            *,
            root_owned: bool = False,
        ) -> bool:
            if root_owned and directory == self.system_directory:
                return True
            return original_trust(
                identity,
                directory,
                root_owned=root_owned,
            )

        with (
            mock.patch.object(
                module,
                "_trusted_non_symlink_directory",
                side_effect=trust_test_system,
            ),
            mock.patch.object(
                module,
                "_strict_root_owned_system_executable",
                return_value=True,
            ),
        ):
            selected = self.select_path()

        self.assertEqual(
            selected,
            os.pathsep.join(
                (
                    str(self.system_directory),
                    str(self.home / ".cargo/bin"),
                    str(self.system_directory),
                )
            ),
        )
        self.assertEqual(self.probe_commands, [])

    def test_system_symlinks_cannot_admit_user_owned_writable_targets(
        self,
    ) -> None:
        target_directory = self.root / "invoking-user-tools"
        target_directory.mkdir(mode=0o775)
        target_node = target_directory / "node"
        target_node.write_text(
            "#!/bin/sh\nprintf '%s\\n' v22.20.0\n",
            encoding="utf-8",
        )
        target_node.chmod(0o755)
        target_npm = target_directory / "npm"
        target_npm.write_text("#!/usr/bin/env node\n", encoding="utf-8")
        target_npm.chmod(0o755)
        system_node = self.system_directory / "node"
        system_npm = self.system_directory / "npm"
        system_node.symlink_to(target_node)
        system_npm.symlink_to(target_npm)
        self.runtime_outputs[system_node] = "v22.20.0"
        module = require_module()
        original_trust = module._trusted_non_symlink_directory

        def trust_test_system(
            identity: object,
            directory: Path,
            *,
            root_owned: bool = False,
        ) -> bool:
            if root_owned and directory == self.system_directory:
                return True
            return original_trust(
                identity,
                directory,
                root_owned=root_owned,
            )

        with (
            mock.patch.object(
                module,
                "_trusted_non_symlink_directory",
                side_effect=trust_test_system,
            ),
            self.assertRaisesRegex(
                module.QualificationError,
                "supported Node 22 or 24",
            ),
        ):
            self.select_path()
        self.assertEqual(self.probe_commands, [])

    def test_system_target_metadata_rejects_every_nonroot_writer(self) -> None:
        module = require_module()
        root_owned = SimpleNamespace(
            st_uid=0,
            st_mode=stat.S_IFREG | 0o755,
        )
        invoking_user_owned = SimpleNamespace(
            st_uid=self.uid,
            st_mode=stat.S_IFREG | 0o755,
        )
        group_writable = SimpleNamespace(
            st_uid=0,
            st_mode=stat.S_IFREG | 0o775,
        )
        world_writable = SimpleNamespace(
            st_uid=0,
            st_mode=stat.S_IFREG | 0o757,
        )

        self.assertTrue(module._root_owned_without_nonroot_writes(root_owned))
        for metadata in (
            invoking_user_owned,
            group_writable,
            world_writable,
        ):
            with self.subTest(metadata=metadata):
                self.assertFalse(
                    module._root_owned_without_nonroot_writes(metadata)
                )

    def test_malformed_names_purporting_supported_majors_fail_closed(self) -> None:
        malformed_names = (
            "v22",
            "v22.1",
            "v22.01.0",
            "v024.1.0",
            "v24.1.0-rc.1",
            "v24garbage",
            f"v24.{'9' * 11}.0",
        )
        for index, name in enumerate(malformed_names):
            with self.subTest(name=name):
                version_root = (
                    self.home
                    / f"case-{index}/.nvm/versions/node"
                )
                version_root.mkdir(parents=True)
                (version_root / name).mkdir()
                original_home = self.account.pw_dir
                self.account.pw_dir = str(version_root.parents[2])
                try:
                    with self.assertRaisesRegex(
                        require_module().QualificationError,
                        "malformed supported NVM Node version",
                    ):
                        self.select_path()
                finally:
                    self.account.pw_dir = original_home

    def test_enormous_supported_looking_major_is_classified_without_int(
        self,
    ) -> None:
        module = require_module()
        name = f"v{'0' * 5000}24.1.0"

        self.assertIsNone(module._nvm_version(name))
        self.assertTrue(module._purports_supported_node_major(name))

    def test_supported_installations_require_coherent_node_and_npm(self) -> None:
        cases = (
            {"include_node": False, "include_npm": True},
            {"include_node": True, "include_npm": False},
        )
        for index, options in enumerate(cases):
            with self.subTest(options=options):
                version = f"v24.{index}.0"
                self.install_nvm_version(version, **options)
                with self.assertRaisesRegex(
                    require_module().QualificationError,
                    "supported NVM Node installation is untrusted or incomplete",
                ):
                    self.select_path()

    def test_hostile_environment_cannot_select_a_different_nvm_home(self) -> None:
        expected = self.install_nvm_version("v22.19.1")
        hostile_home = self.root / "hostile-home"
        hostile_version = hostile_home / ".nvm/versions/node/v24.99.0/bin"
        hostile_version.mkdir(parents=True)
        for executable in ("node", "npm"):
            path = hostile_version / executable
            path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            path.chmod(0o755)

        selected = self.select_path(
            environment={
                "HOME": str(hostile_home),
                "NVM_BIN": str(hostile_version),
                "NVM_DIR": str(hostile_home / ".nvm"),
                "PATH": str(hostile_version),
            }
        )

        self.assertEqual(selected.split(os.pathsep)[0], str(expected / "bin"))
        self.assertNotIn(str(hostile_home), selected)

    def test_qualifier_shape_scan_never_executes_invoking_user_node(self) -> None:
        self.install_nvm_version("v24.18.0")

        self.select_path()

        self.assertEqual(self.probe_commands, [])

    def test_group_writes_require_the_invoking_primary_group(self) -> None:
        self.install_nvm_version("v24.18.0")
        self.assertIn("v24.18.0/bin", self.select_path())

        with self.assertRaisesRegex(
            require_module().QualificationError,
            "untrusted or incomplete",
        ):
            self.select_path(primary_gid=self.gid + 1)

    def test_directory_file_and_traversal_permissions_fail_closed(self) -> None:
        cases = ("directory", "node", "npm-target", "traversal")
        for index, case in enumerate(cases):
            with self.subTest(case=case):
                version = f"v24.{index}.0"
                version_directory = self.install_nvm_version(version)
                if case == "directory":
                    (version_directory / "bin").chmod(0o777)
                elif case == "node":
                    (version_directory / "bin/node").chmod(0o644)
                elif case == "npm-target":
                    (
                        version_directory
                        / "lib/node_modules/npm/bin/npm-cli.js"
                    ).chmod(0o777)
                else:
                    (self.home / ".nvm").chmod(0o666)
                    self.addCleanup((self.home / ".nvm").chmod, 0o775)
                with self.assertRaisesRegex(
                    require_module().QualificationError,
                    "untrusted or incomplete",
                ):
                    self.select_path()
                if case != "traversal":
                    for child in (
                        self.home / ".nvm/versions/node"
                    ).iterdir():
                        if child.name != version:
                            child.rename(child.with_name(f"ignored-v23-{child.name}"))

    def test_symlinked_layout_and_escaping_npm_target_fail_closed(self) -> None:
        actual = self.root / "actual-v24"
        actual.mkdir()
        version_root = self.home / ".nvm/versions/node"
        version_root.mkdir(parents=True)
        (version_root / "v24.18.0").symlink_to(
            actual,
            target_is_directory=True,
        )
        with self.assertRaisesRegex(
            require_module().QualificationError,
            "untrusted or incomplete",
        ):
            self.select_path()

        (version_root / "v24.18.0").unlink()
        version_directory = self.install_nvm_version("v24.18.0")
        npm = version_directory / "bin/npm"
        npm.unlink()
        external_target = self.root / "outside/npm-cli.js"
        external_target.parent.mkdir()
        external_target.write_text("#!/usr/bin/env node\n", encoding="utf-8")
        external_target.chmod(0o755)
        npm.symlink_to(os.path.relpath(external_target, npm.parent))
        with self.assertRaisesRegex(
            require_module().QualificationError,
            "untrusted or incomplete",
        ):
            self.select_path()

    def test_runtime_execution_is_deferred_to_lane7_package_use(
        self,
    ) -> None:
        version_directory = self.install_nvm_version("v24.18.0")
        node = version_directory / "bin/node"
        self.runtime_outputs[node] = subprocess.TimeoutExpired(
            ("node", "--version"),
            5,
        )

        selected = self.select_path()

        self.assertEqual(
            selected.split(os.pathsep)[0],
            str(version_directory / "bin"),
        )
        self.assertEqual(self.probe_commands, [])

    def test_nvm_scan_rejects_too_many_entries_before_sorting(self) -> None:
        versions_root = self.home / ".nvm/versions/node"
        versions_root.mkdir(parents=True)
        for index in range(65):
            (versions_root / f"unsupported-{index:03d}").mkdir()

        with self.assertRaisesRegex(
            require_module().QualificationError,
            "too many NVM Node entries",
        ):
            self.select_path()

    def test_nvm_scan_deletion_race_is_a_typed_failure(self) -> None:
        version_directory = self.install_nvm_version("v24.18.0")
        versions_root = version_directory.parent
        original_iterdir = Path.iterdir

        def racing_iterdir(path: Path) -> object:
            if path != versions_root:
                return original_iterdir(path)

            def entries() -> object:
                yield version_directory
                (version_directory / "bin/node").unlink()

            return entries()

        with (
            mock.patch.object(Path, "iterdir", racing_iterdir),
            self.assertRaisesRegex(
                require_module().QualificationError,
                "untrusted or incomplete",
            ),
        ):
            self.select_path()


class RecordingLaneRunner:
    """Write deterministic lane logs and optionally fail one lane."""

    def __init__(
        self,
        *,
        fail_lane: str | None = None,
        fail_status: int = 1,
        quickstart_output: str | None = None,
        observation: object | None = None,
    ) -> None:
        self.fail_lane = fail_lane
        self.fail_status = fail_status
        self.quickstart_output = (
            expected_quickstart_summary()
            if quickstart_output is None
            else quickstart_output
        )
        self.observation = observation
        self.seen: list[object] = []

    def __call__(self, lane: object, log_path: Path) -> int:
        self.seen.append(lane)
        if self.observation is not None:
            self.observation(lane)
        output = f"{lane.name} passed\n"
        if lane.name == "public-quickstarts":
            output = self.quickstart_output
        log_path.write_text(output, encoding="utf-8")
        if lane.name == self.fail_lane:
            return self.fail_status
        return 0


class QualificationContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = require_module()
        self.temporary = tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-qualification-test-"
        )
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.heavy_lock = self.root / "heavy.lock"
        self.session_lock = self.root / "session.lock"
        self.evidence = self.root / "evidence"
        self.reviewed_package_tool_path = (
            f"{Path.home()}/.cargo/bin:{self.module.TRUSTED_SYSTEM_PATH}"
        )
        package_path_patch = mock.patch.object(
            self.module,
            "_package_tool_path",
            return_value=self.reviewed_package_tool_path,
        )
        package_path_patch.start()
        self.addCleanup(package_path_patch.stop)

    def qualify(
        self,
        runner: object,
        *,
        inspector: object = valid_inspector,
        current_source: object | None = None,
        environment: dict[str, str] | None = None,
        evidence: Path | None = None,
    ) -> object:
        return self.module.qualify(
            PRODUCTION_ID,
            FIXTURE_ID,
            self.evidence if evidence is None else evidence,
            repository_root=REPOSITORY_ROOT,
            python_executable=sys.executable,
            heavy_lock_path=self.heavy_lock,
            session_lock_path=self.session_lock,
            inspect_image=inspector,
            source_probe=lambda: (
                source_identity() if current_source is None else current_source
            ),
            lane_runner=runner,
            environment={} if environment is None else environment,
        )

    def test_frozen_lane_table_binds_exact_ids_environment_and_lock_modes(
        self,
    ) -> None:
        pair = self.module.ExactImagePair(
            production=image_metadata(PRODUCTION_ID, fixture=False),
            fixture=image_metadata(FIXTURE_ID, fixture=True),
            source=source_identity(),
        )
        lanes = self.module.qualification_lanes(
            REPOSITORY_ROOT,
            sys.executable,
            pair,
        )
        self.assertEqual(
            [
                (
                    lane.name,
                    lane.image_id,
                    lane.command,
                    lane.environment,
                    lane.lock_mode,
                    lane.timeout_seconds,
                    lane.priority,
                    lane.cleanup_grace_seconds,
                )
                for lane in lanes
            ],
            [
                (
                    "phase5-atspi-live",
                    FIXTURE_ID,
                    (
                        sys.executable,
                        str(
                            REPOSITORY_ROOT
                            / "scripts/container/test-phase5-atspi-live.py"
                        ),
                        FIXTURE_ID,
                    ),
                    (),
                    self.module.LockMode.OUTER,
                    25 * 60,
                    (15, 3),
                    45.0,
                ),
                (
                    "production-lifecycle",
                    PRODUCTION_ID,
                    (
                        str(REPOSITORY_ROOT / "scripts/container/test-image.sh"),
                        PRODUCTION_ID,
                    ),
                    (),
                    self.module.LockMode.INNER,
                    35 * 60,
                    (15, 3),
                    45.0,
                ),
                (
                    "phase4-live-fixtures",
                    FIXTURE_ID,
                    (
                        sys.executable,
                        str(
                            REPOSITORY_ROOT
                            / "scripts/container/test-phase4-live-fixtures.py"
                        ),
                        FIXTURE_ID,
                    ),
                    (),
                    self.module.LockMode.OUTER,
                    20 * 60,
                    (15, 3),
                    45.0,
                ),
                (
                    "phase4-event-flood",
                    PRODUCTION_ID,
                    (
                        str(
                            REPOSITORY_ROOT
                            / "scripts/container/test-phase4-event-flood.sh"
                        ),
                        PRODUCTION_ID,
                    ),
                    (),
                    self.module.LockMode.INNER,
                    10 * 60,
                    (15, 3),
                    45.0,
                ),
                (
                    "novnc",
                    PRODUCTION_ID,
                    (
                        str(
                            REPOSITORY_ROOT
                            / "scripts/container/test-novnc-spike.sh"
                        ),
                    ),
                    (("XENOTEER_NOVNC_SPIKE_BASE_IMAGE", PRODUCTION_ID),),
                    self.module.LockMode.OUTER,
                    10 * 60,
                    (15, 3),
                    45.0,
                ),
                (
                    "desktop-app-matrix",
                    FIXTURE_ID,
                    (
                        str(
                            REPOSITORY_ROOT
                            / "scripts/container/test-desktop-app-image.sh"
                        ),
                        FIXTURE_ID,
                    ),
                    (),
                    self.module.LockMode.OUTER,
                    25 * 60,
                    (15, 3),
                    45.0,
                ),
                (
                    "public-quickstarts",
                    FIXTURE_ID,
                    (
                        sys.executable,
                        str(
                            REPOSITORY_ROOT
                            / "scripts/sdk/test-public-quickstarts.py"
                        ),
                        FIXTURE_ID,
                    ),
                    (
                        (
                            "XENOTEER_PACKAGE_BUILD_PATH",
                            self.module.TRUSTED_SYSTEM_PATH,
                        ),
                    ),
                    self.module.LockMode.INNER,
                    20 * 60,
                    (15, 3),
                    45.0,
                ),
            ],
        )
        self.assertEqual(
            [lane.image_id for lane in lanes],
            [
                FIXTURE_ID,
                PRODUCTION_ID,
                FIXTURE_ID,
                PRODUCTION_ID,
                PRODUCTION_ID,
                FIXTURE_ID,
                FIXTURE_ID,
            ],
        )
        self.assertEqual(
            [lane.lock_mode for lane in lanes],
            [
                self.module.LockMode.OUTER,
                self.module.LockMode.INNER,
                self.module.LockMode.OUTER,
                self.module.LockMode.INNER,
                self.module.LockMode.OUTER,
                self.module.LockMode.OUTER,
                self.module.LockMode.INNER,
            ],
        )
        self.assertEqual(
            dict(lanes[4].environment),
            {"XENOTEER_NOVNC_SPIKE_BASE_IMAGE": PRODUCTION_ID},
        )
        for index, lane in enumerate(lanes):
            with self.subTest(index=index, lane=lane.name):
                if lane.name != "novnc":
                    self.assertIn(lane.image_id, lane.command)
                self.assertNotIn("production:candidate", lane.command)
                self.assertNotIn("fixture:candidate", lane.command)
                self.assertGreater(lane.timeout_seconds, 0)
                self.assertEqual(lane.priority, (15, 3))

    def test_held_heavy_lock_rejects_before_inspection_or_any_lane(self) -> None:
        inspections: list[tuple[str, str]] = []
        lane_runner = RecordingLaneRunner()
        self.heavy_lock.touch()
        with self.heavy_lock.open("r+b") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(
                self.module.QualificationError,
                "heavy-build lock",
            ):
                self.qualify(
                    lane_runner,
                    inspector=lambda production, fixture: inspections.append(
                        (production, fixture)
                    ),
                )
        self.assertEqual(inspections, [])
        self.assertEqual(lane_runner.seen, [])
        self.assertFalse(self.evidence.exists())

    def test_lane2_can_acquire_its_nested_heavy_lock(self) -> None:
        nested_acquired: list[str] = []

        def observe(lane: object) -> None:
            if lane.name != "production-lifecycle":
                return
            with self.heavy_lock.open("a+b") as nested:
                fcntl.flock(nested, fcntl.LOCK_EX | fcntl.LOCK_NB)
                nested_acquired.append(lane.name)
                fcntl.flock(nested, fcntl.LOCK_UN)

        self.qualify(RecordingLaneRunner(observation=observe))
        self.assertEqual(nested_acquired, ["production-lifecycle"])

    def test_outer_and_inner_lane_lock_ownership_is_observable(self) -> None:
        observed: dict[str, bool] = {}

        def observe(lane: object) -> None:
            with self.heavy_lock.open("a+b") as contender:
                try:
                    fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    observed[lane.name] = True
                else:
                    observed[lane.name] = False
                    fcntl.flock(contender, fcntl.LOCK_UN)

        self.qualify(RecordingLaneRunner(observation=observe))
        self.assertEqual(
            observed,
            {
                "phase5-atspi-live": True,
                "production-lifecycle": False,
                "phase4-live-fixtures": True,
                "phase4-event-flood": False,
                "novnc": True,
                "desktop-app-matrix": True,
                "public-quickstarts": False,
            },
        )

    def test_first_failure_stops_every_possible_lane(self) -> None:
        names = [
            "phase5-atspi-live",
            "production-lifecycle",
            "phase4-live-fixtures",
            "phase4-event-flood",
            "novnc",
            "desktop-app-matrix",
            "public-quickstarts",
        ]
        for failed_index, failed_name in enumerate(names):
            with self.subTest(failed_name=failed_name):
                evidence = self.root / f"failure-{failed_index}"
                runner = RecordingLaneRunner(
                    fail_lane=failed_name,
                    fail_status=77,
                )
                with self.assertRaisesRegex(
                    self.module.QualificationError,
                    failed_name,
                ):
                    self.qualify(runner, evidence=evidence)
                self.assertEqual(
                    [lane.name for lane in runner.seen],
                    names[: failed_index + 1],
                )
                self.assertFalse((evidence / "qualification.json").exists())
                attempt = json.loads(
                    (evidence / "attempt.json").read_text(encoding="utf-8")
                )
                self.assertEqual(attempt["status"], "rejected")
                self.assertEqual(attempt["failed_lane"], failed_name)
                self.assertEqual(attempt["exit_status"], 77)

    def test_unexpected_lane_runner_exception_is_atomically_rejected(self) -> None:
        def explode(lane: object, log_path: Path) -> int:
            del lane, log_path
            raise RuntimeError("Bearer must-not-leak")

        with self.assertRaisesRegex(
            self.module.QualificationError,
            "phase5-atspi-live.*runner failed",
        ):
            self.qualify(explode)

        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertEqual(attempt["failed_lane"], "phase5-atspi-live")
        self.assertEqual(attempt["exit_status"], "runner-error")
        self.assertEqual(len(attempt["lanes"]), 1)
        self.assertEqual(attempt["lanes"][0]["status"], "rejected")
        self.assertEqual(
            attempt["lanes"][0]["exit_status"],
            "runner-error",
        )
        self.assertRegex(attempt["lanes"][0]["log_sha256"], r"\A[0-9a-f]{64}\Z")
        log_path = self.evidence / "lane-01-phase5-atspi-live.log"
        self.assertTrue(log_path.is_file())
        self.assertEqual(stat.S_IMODE(log_path.stat().st_mode), 0o600)
        self.assertNotIn("must-not-leak", log_path.read_text(encoding="utf-8"))

    def test_lane_runner_interrupt_is_recorded_before_propagation(self) -> None:
        def interrupt(lane: object, log_path: Path) -> int:
            del lane, log_path
            raise KeyboardInterrupt

        with self.assertRaises(KeyboardInterrupt):
            self.qualify(interrupt)

        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertEqual(attempt["failed_lane"], "phase5-atspi-live")
        self.assertEqual(attempt["exit_status"], "interrupted")
        self.assertEqual(len(attempt["lanes"]), 1)
        self.assertEqual(attempt["lanes"][0]["status"], "rejected")
        self.assertRegex(attempt["lanes"][0]["log_sha256"], r"\A[0-9a-f]{64}\Z")

    def test_rejects_daemon_overrides_before_image_inspection(self) -> None:
        for variable in (
            "XENOTEERD_BINARY_OVERRIDE",
            "XENOTEER_TEST_DAEMON_BINARY",
        ):
            with self.subTest(variable=variable):
                inspections: list[tuple[str, str]] = []
                runner = RecordingLaneRunner()
                with self.assertRaisesRegex(
                    self.module.QualificationError,
                    variable,
                ):
                    self.qualify(
                        runner,
                        inspector=lambda production, fixture: inspections.append(
                            (production, fixture)
                        ),
                        environment={variable: ""},
                        evidence=self.root / f"override-{variable}",
                    )
                self.assertEqual(inspections, [])
                self.assertEqual(runner.seen, [])

    def test_actual_lane_processes_receive_only_reviewed_full_scope_environment(
        self,
    ) -> None:
        captured: list[tuple[str, dict[str, str], dict[str, str]]] = []

        def run_lane(
            lane: object,
            log_path: Path,
            *,
            cwd: Path,
            environment: dict[str, str],
        ) -> int:
            del cwd
            captured.append(
                (lane.name, dict(environment), dict(lane.environment))
            )
            output = f"{lane.name} passed\n"
            if lane.name == "public-quickstarts":
                output = expected_quickstart_summary()
            log_path.write_text(output, encoding="utf-8")
            return 0

        hostile = {
            "BASH_ENV": "/tmp/attacker.sh",
            "DOCKER_CONFIG": "/tmp/docker",
            "DOCKER_HOST": "tcp://attacker.invalid:2375",
            "ENV": "/tmp/attacker.sh",
            "HOME": "/tmp/attacker-home",
            "LD_PRELOAD": "/tmp/attacker.so",
            "PATH": "/tmp/attacker-bin:/usr/bin",
            "XENOTEER_PACKAGE_BUILD_PATH": "/tmp/attacker-package-bin",
            "PYTHONHOME": "/tmp/python",
            "PYTHONPATH": "/tmp/python",
            "XENOTEER_DESKTOP_MATRIX_SCOPE": "hardened-only",
            "XENOTEER_UNREVIEWED_BYPASS": "1",
        }
        with mock.patch.object(
            self.module,
            "run_lane_process",
            side_effect=run_lane,
        ):
            self.module.qualify(
                PRODUCTION_ID,
                FIXTURE_ID,
                self.evidence,
                repository_root=REPOSITORY_ROOT,
                python_executable=sys.executable,
                heavy_lock_path=self.heavy_lock,
                session_lock_path=self.session_lock,
                inspect_image=valid_inspector,
                source_probe=source_identity,
                environment=hostile,
            )
        self.assertEqual(len(captured), 7)
        forbidden = set(hostile) - {
            "HOME",
            "PATH",
            "XENOTEER_DESKTOP_MATRIX_SCOPE",
        }
        for lane_name, lane_environment, lane_overrides in captured:
            with self.subTest(lane=lane_name, environment=lane_environment):
                self.assertTrue(forbidden.isdisjoint(lane_environment))
                self.assertEqual(
                    lane_environment["XENOTEER_DESKTOP_MATRIX_SCOPE"],
                    "full",
                )
                self.assertNotIn("/tmp", lane_environment["PATH"])
                self.assertTrue(Path(lane_environment["HOME"]).is_absolute())
                self.assertEqual(
                    lane_environment["PATH"],
                    self.module.TRUSTED_SYSTEM_PATH,
                )
                if lane_name == "public-quickstarts":
                    self.assertEqual(
                        lane_overrides["XENOTEER_PACKAGE_BUILD_PATH"],
                        self.reviewed_package_tool_path,
                    )
                    self.assertEqual(
                        lane_environment["PATH"],
                        self.module.TRUSTED_SYSTEM_PATH,
                    )
                else:
                    self.assertNotIn(
                        "XENOTEER_PACKAGE_BUILD_PATH",
                        lane_overrides,
                    )

    def test_rejects_invalid_image_identity_and_ancestry_before_lane1(self) -> None:
        cases = {
            "malformed production ID": (
                image_metadata("production:mutable", fixture=False),
                image_metadata(FIXTURE_ID, fixture=True),
                source_identity(),
            ),
            "fixture base mismatch": (
                image_metadata(PRODUCTION_ID, fixture=False),
                image_metadata(
                    FIXTURE_ID,
                    fixture=True,
                    base_id="sha256:" + ("77" * 32),
                ),
                source_identity(),
            ),
            "layer prefix mismatch": (
                image_metadata(PRODUCTION_ID, fixture=False),
                image_metadata(
                    FIXTURE_ID,
                    fixture=True,
                    layers=("different", "fixture-layer"),
                ),
                source_identity(),
            ),
            "dirty production": (
                image_metadata(PRODUCTION_ID, fixture=False, dirty="true"),
                image_metadata(FIXTURE_ID, fixture=True),
                source_identity(),
            ),
            "source label mismatch": (
                image_metadata(
                    PRODUCTION_ID,
                    fixture=False,
                    source_tree="88" * 32,
                ),
                image_metadata(FIXTURE_ID, fixture=True),
                source_identity(),
            ),
            "dependency label mismatch": (
                image_metadata(
                    PRODUCTION_ID,
                    fixture=False,
                    dependency_lock="99" * 32,
                ),
                image_metadata(FIXTURE_ID, fixture=True),
                source_identity(),
            ),
            "revision mismatch": (
                image_metadata(
                    PRODUCTION_ID,
                    fixture=False,
                    revision="a0" * 20,
                ),
                image_metadata(FIXTURE_ID, fixture=True),
                source_identity(),
            ),
            "dirty source": (
                image_metadata(PRODUCTION_ID, fixture=False),
                image_metadata(FIXTURE_ID, fixture=True),
                source_identity(clean=False),
            ),
        }
        for index, (name, values) in enumerate(cases.items()):
            with self.subTest(name=name):
                production, fixture, source = values
                runner = RecordingLaneRunner()

                def inspect(
                    production_id: str,
                    fixture_id: str,
                ) -> tuple[object, object]:
                    self.assertEqual(production_id, PRODUCTION_ID)
                    self.assertEqual(fixture_id, FIXTURE_ID)
                    return production, fixture

                with self.assertRaises(self.module.QualificationError):
                    self.qualify(
                        runner,
                        inspector=inspect,
                        current_source=source,
                        evidence=self.root / f"identity-{index}",
                    )
                self.assertEqual(runner.seen, [])

    def test_combined_inspection_occurs_once_and_lanes_keep_exact_ids(self) -> None:
        inspections: list[tuple[str, str]] = []

        def inspect(
            production_id: str,
            fixture_id: str,
        ) -> tuple[object, object]:
            inspections.append((production_id, fixture_id))
            return valid_inspector(production_id, fixture_id)

        runner = RecordingLaneRunner()
        self.qualify(runner, inspector=inspect)
        self.assertEqual(
            inspections,
            [(PRODUCTION_ID, FIXTURE_ID)],
        )
        self.assertEqual(
            [lane.image_id for lane in runner.seen],
            [
                FIXTURE_ID,
                PRODUCTION_ID,
                FIXTURE_ID,
                PRODUCTION_ID,
                PRODUCTION_ID,
                FIXTURE_ID,
                FIXTURE_ID,
            ],
        )

    def test_cli_image_ids_must_be_exact_lowercase_distinct_digests(self) -> None:
        invalid_pairs = (
            ("production:candidate", FIXTURE_ID),
            (PRODUCTION_ID.upper(), FIXTURE_ID),
            ("sha256:1234", FIXTURE_ID),
            (PRODUCTION_ID + "\n", FIXTURE_ID),
            (PRODUCTION_ID, PRODUCTION_ID),
        )
        for index, (production_id, fixture_id) in enumerate(invalid_pairs):
            with self.subTest(
                production_id=production_id,
                fixture_id=fixture_id,
            ):
                inspections: list[tuple[str, str]] = []
                runner = RecordingLaneRunner()
                with self.assertRaises(self.module.QualificationError):
                    self.module.qualify(
                        production_id,
                        fixture_id,
                        self.root / f"invalid-id-{index}",
                        repository_root=REPOSITORY_ROOT,
                        python_executable=sys.executable,
                        heavy_lock_path=self.heavy_lock,
                        session_lock_path=self.session_lock,
                        inspect_image=(
                            lambda production, fixture: inspections.append(
                                (production, fixture)
                            )
                        ),
                        source_probe=source_identity,
                        lane_runner=runner,
                        environment={},
                    )
                self.assertEqual(inspections, [])
                self.assertEqual(runner.seen, [])

    def test_source_identity_drift_after_a_lane_rejects_immediately(self) -> None:
        runner = RecordingLaneRunner()

        def probe() -> object:
            if runner.seen:
                return source_identity(source_tree="ab" * 32)
            return source_identity()

        with self.assertRaisesRegex(
            self.module.QualificationError,
            "source identity changed",
        ):
            self.module.qualify(
                PRODUCTION_ID,
                FIXTURE_ID,
                self.evidence,
                repository_root=REPOSITORY_ROOT,
                python_executable=sys.executable,
                heavy_lock_path=self.heavy_lock,
                session_lock_path=self.session_lock,
                inspect_image=valid_inspector,
                source_probe=probe,
                lane_runner=runner,
                environment={},
            )
        self.assertEqual(
            [lane.name for lane in runner.seen],
            ["phase5-atspi-live"],
        )
        self.assertFalse((self.evidence / "qualification.json").exists())

    def test_source_probe_exception_after_lane_is_atomically_rejected(self) -> None:
        runner = RecordingLaneRunner()
        probe_count = 0

        def probe() -> object:
            nonlocal probe_count
            probe_count += 1
            if probe_count >= 3:
                raise RuntimeError("Bearer must-not-leak")
            return source_identity()

        with self.assertRaisesRegex(
            self.module.QualificationError,
            "phase5-atspi-live.*evidence failed",
        ):
            self.module.qualify(
                PRODUCTION_ID,
                FIXTURE_ID,
                self.evidence,
                repository_root=REPOSITORY_ROOT,
                python_executable=sys.executable,
                heavy_lock_path=self.heavy_lock,
                session_lock_path=self.session_lock,
                inspect_image=valid_inspector,
                source_probe=probe,
                lane_runner=runner,
                environment={},
            )
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertEqual(attempt["failed_lane"], "phase5-atspi-live")
        self.assertEqual(attempt["lanes"][0]["status"], "rejected")
        self.assertFalse((self.evidence / "qualification.json").exists())

    def test_lane_log_hash_failure_is_atomically_rejected(self) -> None:
        original_hash = self.module._hash_file

        def fail_log_hash(path: Path) -> str:
            if path.suffix == ".log":
                raise OSError("injected hash failure")
            return original_hash(path)

        with (
            mock.patch.object(
                self.module,
                "_hash_file",
                side_effect=fail_log_hash,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "phase5-atspi-live.*evidence failed",
            ),
        ):
            self.qualify(RecordingLaneRunner())
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertEqual(attempt["lanes"][0]["log_sha256"], None)
        self.assertFalse((self.evidence / "qualification.json").exists())

    def test_lane_log_chmod_failure_is_atomically_rejected(self) -> None:
        original_chmod = self.module.os.chmod

        def fail_log_chmod(path: Path, mode: int) -> None:
            if Path(path).suffix == ".log":
                raise OSError("injected chmod failure")
            original_chmod(path, mode)

        with (
            mock.patch.object(
                self.module.os,
                "chmod",
                side_effect=fail_log_chmod,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "phase5-atspi-live.*evidence failed",
            ),
        ):
            self.qualify(RecordingLaneRunner())
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertEqual(attempt["failed_lane"], "phase5-atspi-live")
        self.assertEqual(attempt["lanes"][0]["log_sha256"], None)
        self.assertFalse((self.evidence / "qualification.json").exists())

    def test_final_manifest_write_failure_cannot_leave_passed_attempt(self) -> None:
        original_atomic_json = self.module._atomic_json

        def fail_qualification(
            path: Path,
            value: dict[str, object],
            *,
            exclusive: bool,
        ) -> None:
            if path.name == "qualification.json":
                raise OSError("injected final write failure")
            original_atomic_json(path, value, exclusive=exclusive)

        with (
            mock.patch.object(
                self.module,
                "_atomic_json",
                side_effect=fail_qualification,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "final qualification evidence failed",
            ),
        ):
            self.qualify(RecordingLaneRunner())
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertFalse((self.evidence / "qualification.json").exists())

    def test_final_directory_fsync_failure_rolls_back_authority(self) -> None:
        calls = 0
        original_fsync_directory = self.module._fsync_directory

        def fail_final_fsync(path: Path) -> None:
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("injected directory fsync failure")
            original_fsync_directory(path)

        with (
            mock.patch.object(
                self.module,
                "_fsync_directory",
                side_effect=fail_final_fsync,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "final qualification evidence failed",
            ),
        ):
            self.qualify(RecordingLaneRunner())
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertFalse((self.evidence / "qualification.json").exists())

    def test_oversized_lane7_log_is_rejected_before_summary_read(self) -> None:
        class OversizedQuickstartRunner(RecordingLaneRunner):
            def __call__(self, lane: object, log_path: Path) -> int:
                if lane.name == "public-quickstarts":
                    self.seen.append(lane)
                    log_path.write_bytes(b"x" * 2048)
                    return 0
                return super().__call__(lane, log_path)

        with (
            mock.patch.object(self.module, "MAX_LANE_LOG_BYTES", 1024),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "public-quickstarts.*log.*limit",
            ),
        ):
            self.qualify(OversizedQuickstartRunner())
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "rejected")
        self.assertEqual(attempt["failed_lane"], "public-quickstarts")

    def test_session_lock_rejects_concurrent_runner_before_inspection(self) -> None:
        inspections: list[tuple[str, str]] = []
        runner = RecordingLaneRunner()
        self.session_lock.touch()
        with self.session_lock.open("r+b") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(
                self.module.QualificationError,
                "qualification.*running",
            ):
                self.qualify(
                    runner,
                    inspector=lambda production, fixture: inspections.append(
                        (production, fixture)
                    ),
                )
        self.assertEqual(inspections, [])
        self.assertEqual(runner.seen, [])

    def test_success_evidence_is_private_exclusive_and_complete(self) -> None:
        runner = RecordingLaneRunner()
        result = self.qualify(runner)
        self.assertEqual(result["production_image"], PRODUCTION_ID)
        self.assertEqual(result["fixture_image"], FIXTURE_ID)
        self.assertEqual(result["source_tree"], SOURCE_TREE)
        self.assertEqual(
            stat.S_IMODE(self.evidence.stat().st_mode),
            0o700,
        )
        for path in (
            self.evidence / "attempt.json",
            self.evidence / "qualification.json",
        ):
            self.assertTrue(path.is_file())
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            json.loads(path.read_text(encoding="utf-8"))
        attempt = json.loads(
            (self.evidence / "attempt.json").read_text(encoding="utf-8")
        )
        qualification = json.loads(
            (self.evidence / "qualification.json").read_text(encoding="utf-8")
        )
        self.assertEqual(attempt["status"], "lanes-passed")
        self.assertEqual(qualification["status"], "passed")
        self.assertEqual(
            qualification["attempt_sha256"],
            self.module._hash_file(self.evidence / "attempt.json"),
        )
        self.assertEqual(list(self.evidence.glob("*.tmp")), [])
        before = {
            path.name: path.read_bytes()
            for path in self.evidence.iterdir()
            if path.is_file()
        }
        with self.assertRaisesRegex(
            self.module.QualificationError,
            "evidence.*exists",
        ):
            self.qualify(RecordingLaneRunner())
        after = {
            path.name: path.read_bytes()
            for path in self.evidence.iterdir()
            if path.is_file()
        }
        self.assertEqual(after, before)

    def test_attempt_manifest_is_always_valid_during_atomic_updates(self) -> None:
        observed_states: list[dict[str, object]] = []

        def observe(_: object) -> None:
            observed_states.append(
                json.loads(
                    (self.evidence / "attempt.json").read_text(encoding="utf-8")
                )
            )

        self.qualify(RecordingLaneRunner(observation=observe))
        self.assertEqual(len(observed_states), 7)
        self.assertTrue(all(state["status"] == "running" for state in observed_states))
        self.assertEqual(list(self.evidence.glob("*.tmp")), [])

    def test_partial_or_forged_quickstart_summary_cannot_create_success(
        self,
    ) -> None:
        cases = {
            "missing digest": expected_quickstart_summary(omit="rust_sdk"),
            "wrong production": expected_quickstart_summary(
                production="sha256:" + ("aa" * 32)
            ),
            "wrong fixture": expected_quickstart_summary(
                fixture="sha256:" + ("bb" * 32)
            ),
            "wrong source": expected_quickstart_summary(source_tree="cc" * 32),
            "duplicate summary": (
                expected_quickstart_summary() + expected_quickstart_summary()
            ),
            "success text without prefix": expected_quickstart_summary().replace(
                "public quick-start qualification passed: ",
                "",
            ),
        }
        for index, (name, output) in enumerate(cases.items()):
            with self.subTest(name=name):
                evidence = self.root / f"forged-{index}"
                runner = RecordingLaneRunner(quickstart_output=output)
                with self.assertRaises(self.module.QualificationError):
                    self.qualify(runner, evidence=evidence)
                self.assertEqual(len(runner.seen), 7)
                self.assertFalse((evidence / "qualification.json").exists())
                attempt = json.loads(
                    (evidence / "attempt.json").read_text(encoding="utf-8")
                )
                self.assertEqual(attempt["status"], "rejected")
                self.assertEqual(attempt["failed_lane"], "public-quickstarts")

    def test_secrets_in_quickstart_summary_are_rejected(self) -> None:
        output = expected_quickstart_summary() + "Authorization=Bearer secret\n"
        with self.assertRaises(self.module.QualificationError):
            self.qualify(RecordingLaneRunner(quickstart_output=output))
        self.assertFalse((self.evidence / "qualification.json").exists())


class BoundedProcessContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = require_module()

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_timeout_terminates_the_complete_process_group(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-process-test-"
        ) as temporary:
            root = Path(temporary)
            child_term = root / "child-terminated"
            child_code = textwrap.dedent(
                """
                import pathlib
                import signal
                import sys
                import time

                marker = pathlib.Path(sys.argv[1])

                def terminate(signum, frame):
                    del signum, frame
                    marker.write_text("terminated", encoding="utf-8")
                    raise SystemExit(0)

                signal.signal(signal.SIGTERM, terminate)
                while True:
                    time.sleep(0.05)
                """
            )
            parent_code = textwrap.dedent(
                """
                import subprocess
                import sys
                import time

                subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2]])
                while True:
                    time.sleep(0.05)
                """
            )
            lane = self.module.Lane(
                name="timeout-probe",
                image_id=PRODUCTION_ID,
                command=(
                    sys.executable,
                    "-c",
                    parent_code,
                    child_code,
                    str(child_term),
                ),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=0.25,
                priority=(15, 3),
            )
            started = time.monotonic()
            with self.assertRaisesRegex(
                self.module.QualificationError,
                "timeout-probe.*timed out",
            ):
                self.module.run_lane_process(
                    lane,
                    root / "timeout.log",
                    cwd=root,
                    environment={},
                    terminate_grace_seconds=0.5,
                )
            self.assertLess(time.monotonic() - started, 2.0)
            deadline = time.monotonic() + 1.0
            while time.monotonic() < deadline and not child_term.exists():
                time.sleep(0.01)
            self.assertEqual(
                child_term.read_text(encoding="utf-8"),
                "terminated",
            )

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_timeout_escalates_to_sigkill_for_term_ignoring_group(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-sigkill-test-"
        ) as temporary:
            root = Path(temporary)
            process_group = root / "process-group"
            child_code = textwrap.dedent(
                """
                import signal
                import time

                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                while True:
                    time.sleep(0.05)
                """
            )
            parent_code = textwrap.dedent(
                """
                import os
                import pathlib
                import signal
                import subprocess
                import sys
                import time

                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                pathlib.Path(sys.argv[2]).write_text(
                    str(os.getpgrp()), encoding="utf-8"
                )
                subprocess.Popen([sys.executable, "-c", sys.argv[1]])
                while True:
                    time.sleep(0.05)
                """
            )
            lane = self.module.Lane(
                name="sigkill-probe",
                image_id=PRODUCTION_ID,
                command=(
                    sys.executable,
                    "-c",
                    parent_code,
                    child_code,
                    str(process_group),
                ),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=0.2,
                priority=(15, 3),
            )
            with self.assertRaisesRegex(
                self.module.QualificationError,
                "sigkill-probe.*timed out",
            ):
                self.module.run_lane_process(
                    lane,
                    root / "sigkill.log",
                    cwd=root,
                    environment={},
                    terminate_grace_seconds=0.15,
                )
            group_id = int(process_group.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 1.0
            while time.monotonic() < deadline:
                live_members = []
                for status_path in Path("/proc").glob("[0-9]*/stat"):
                    try:
                        status = status_path.read_text(encoding="utf-8")
                    except (FileNotFoundError, PermissionError):
                        continue
                    fields = status.rsplit(")", 1)[-1].split()
                    if len(fields) > 2 and int(fields[2]) == group_id:
                        if fields[0] != "Z":
                            live_members.append(status_path.parent.name)
                if not live_members:
                    break
                time.sleep(0.01)
            self.assertEqual(live_members, [])

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_timeout_kills_ignoring_descendant_after_leader_exits_on_term(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-descendant-test-"
        ) as temporary:
            root = Path(temporary)
            process_group = root / "process-group"
            child_code = textwrap.dedent(
                """
                import signal
                import time

                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                while True:
                    time.sleep(0.05)
                """
            )
            parent_code = textwrap.dedent(
                """
                import os
                import pathlib
                import subprocess
                import sys
                import time

                subprocess.Popen([sys.executable, "-c", sys.argv[1]])
                pathlib.Path(sys.argv[2]).write_text(
                    str(os.getpgrp()), encoding="utf-8"
                )
                while True:
                    time.sleep(0.05)
                """
            )
            lane = self.module.Lane(
                name="descendant-probe",
                image_id=PRODUCTION_ID,
                command=(
                    sys.executable,
                    "-c",
                    parent_code,
                    child_code,
                    str(process_group),
                ),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=0.2,
                priority=(15, 3),
            )
            group_id: int | None = None
            try:
                with self.assertRaisesRegex(
                    self.module.QualificationError,
                    "descendant-probe.*timed out",
                ):
                    self.module.run_lane_process(
                        lane,
                        root / "descendant.log",
                        cwd=root,
                        environment={},
                        terminate_grace_seconds=0.15,
                    )
                group_id = int(process_group.read_text(encoding="utf-8"))
                deadline = time.monotonic() + 1.0
                live_members = ["not-yet-inspected"]
                while time.monotonic() < deadline:
                    live_members = []
                    for status_path in Path("/proc").glob("[0-9]*/stat"):
                        try:
                            status = status_path.read_text(encoding="utf-8")
                        except (FileNotFoundError, PermissionError):
                            continue
                        fields = status.rsplit(")", 1)[-1].split()
                        if len(fields) > 2 and int(fields[2]) == group_id:
                            if fields[0] != "Z":
                                live_members.append(status_path.parent.name)
                    if not live_members:
                        break
                    time.sleep(0.01)
                self.assertEqual(live_members, [])
            finally:
                if group_id is not None:
                    try:
                        os.killpg(group_id, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_cleanup_grace_waits_for_descendant_after_leader_exits(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-cleanup-grace-test-"
        ) as temporary:
            root = Path(temporary)
            child_cleaned = root / "child-cleaned"
            child_code = textwrap.dedent(
                """
                import pathlib
                import signal
                import sys
                import time

                marker = pathlib.Path(sys.argv[1])

                def terminate(signum, frame):
                    del signum, frame
                    time.sleep(0.25)
                    marker.write_text("clean", encoding="utf-8")
                    raise SystemExit(0)

                signal.signal(signal.SIGTERM, terminate)
                while True:
                    time.sleep(0.05)
                """
            )
            parent_code = textwrap.dedent(
                """
                import subprocess
                import sys
                import time

                subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2]])
                while True:
                    time.sleep(0.05)
                """
            )
            lane = self.module.Lane(
                name="cleanup-grace-probe",
                image_id=PRODUCTION_ID,
                command=(
                    sys.executable,
                    "-c",
                    parent_code,
                    child_code,
                    str(child_cleaned),
                ),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=0.2,
                priority=(15, 3),
            )
            with self.assertRaisesRegex(
                self.module.QualificationError,
                "cleanup-grace-probe.*timed out",
            ):
                self.module.run_lane_process(
                    lane,
                    root / "cleanup-grace.log",
                    cwd=root,
                    environment={},
                    terminate_grace_seconds=0.8,
                )
            self.assertEqual(
                child_cleaned.read_text(encoding="utf-8"),
                "clean",
            )

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_lane_output_limit_terminates_process_and_caps_log(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-output-limit-test-"
        ) as temporary:
            root = Path(temporary)
            lane = self.module.Lane(
                name="output-limit-probe",
                image_id=PRODUCTION_ID,
                command=(
                    sys.executable,
                    "-c",
                    "import sys; sys.stdout.write('x' * 8192); sys.stdout.flush()",
                ),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=2,
                priority=(15, 3),
            )
            log_path = root / "output-limit.log"
            with (
                mock.patch.object(self.module, "MAX_LANE_LOG_BYTES", 4096),
                self.assertRaisesRegex(
                    self.module.QualificationError,
                    "output-limit-probe.*evidence limit",
                ),
            ):
                self.module.run_lane_process(
                    lane,
                    log_path,
                    cwd=root,
                    environment={},
                    terminate_grace_seconds=0.2,
                )
            self.assertEqual(log_path.stat().st_size, 4096)

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_successful_leader_cannot_orphan_silent_same_group_descendant(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-orphan-success-test-"
        ) as temporary:
            root = Path(temporary)
            process_group = root / "process-group"
            child_code = textwrap.dedent(
                """
                import signal
                import time

                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                while True:
                    time.sleep(0.05)
                """
            )
            parent_code = textwrap.dedent(
                """
                import os
                import pathlib
                import subprocess
                import sys

                subprocess.Popen(
                    [sys.executable, "-c", sys.argv[1]],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                pathlib.Path(sys.argv[2]).write_text(
                    str(os.getpgrp()), encoding="utf-8"
                )
                """
            )
            lane = self.module.Lane(
                name="orphan-success-probe",
                image_id=PRODUCTION_ID,
                command=(
                    sys.executable,
                    "-c",
                    parent_code,
                    child_code,
                    str(process_group),
                ),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=2,
                priority=(15, 3),
            )
            with self.assertRaisesRegex(
                self.module.QualificationError,
                "orphan-success-probe.*descendant",
            ):
                self.module.run_lane_process(
                    lane,
                    root / "orphan-success.log",
                    cwd=root,
                    environment={},
                    terminate_grace_seconds=0.15,
                )
            group_id = int(process_group.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 1.0
            live_members = ["not-yet-inspected"]
            while time.monotonic() < deadline:
                live_members = []
                for status_path in Path("/proc").glob("[0-9]*/stat"):
                    try:
                        status = status_path.read_text(encoding="utf-8")
                    except (FileNotFoundError, PermissionError):
                        continue
                    fields = status.rsplit(")", 1)[-1].split()
                    if len(fields) > 2 and int(fields[2]) == group_id:
                        if fields[0] != "Z":
                            live_members.append(status_path.parent.name)
                if not live_members:
                    break
                time.sleep(0.01)
            self.assertEqual(live_members, [])

    def test_missing_lane_executable_is_rejected_before_process_creation(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-missing-command-test-"
        ) as temporary:
            root = Path(temporary)
            lane = self.module.Lane(
                name="missing-command",
                image_id=PRODUCTION_ID,
                command=(str(root / "absent"), PRODUCTION_ID),
                environment=(),
                lock_mode=self.module.LockMode.INNER,
                timeout_seconds=1,
                priority=(15, 3),
            )
            with mock.patch.object(
                self.module.subprocess,
                "Popen",
            ) as popen:
                with self.assertRaisesRegex(
                    self.module.QualificationError,
                    "required command is unavailable",
                ):
                    self.module.run_lane_process(
                        lane,
                        root / "missing.log",
                        cwd=root,
                        environment={},
                    )
            popen.assert_not_called()
            log_path = root / "missing.log"
            self.assertTrue(log_path.is_file())
            self.assertEqual(stat.S_IMODE(log_path.stat().st_mode), 0o600)


class HostTrustBoundaryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = require_module()
        self.temporary = tempfile.TemporaryDirectory(
            prefix="xenoteer-phase6-host-trust-test-"
        )
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def test_default_evidence_root_rejects_symlink_without_chmod_target(self) -> None:
        target = self.root / "target"
        target.mkdir(mode=0o755)
        evidence_root = self.root / "evidence"
        evidence_root.symlink_to(target, target_is_directory=True)
        with (
            mock.patch.object(
                self.module,
                "DEFAULT_EVIDENCE_ROOT",
                evidence_root,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "evidence root",
            ),
        ):
            self.module._default_evidence_directory()
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o755)

    def test_default_evidence_root_rejects_unsafe_mode_without_repair(self) -> None:
        evidence_root = self.root / "evidence"
        evidence_root.mkdir(mode=0o755)
        with (
            mock.patch.object(
                self.module,
                "DEFAULT_EVIDENCE_ROOT",
                evidence_root,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "evidence root",
            ),
        ):
            self.module._default_evidence_directory()
        self.assertEqual(stat.S_IMODE(evidence_root.stat().st_mode), 0o755)

    def test_default_evidence_root_rejects_wrong_owner(self) -> None:
        evidence_root = self.root / "evidence"
        evidence_root.mkdir(mode=0o700)
        with (
            mock.patch.object(
                self.module,
                "DEFAULT_EVIDENCE_ROOT",
                evidence_root,
            ),
            mock.patch.object(
                self.module.os,
                "geteuid",
                return_value=evidence_root.stat().st_uid + 1,
            ),
            self.assertRaisesRegex(
                self.module.QualificationError,
                "evidence root",
            ),
        ):
            self.module._default_evidence_directory()

    def test_default_evidence_root_accepts_private_owned_directory(self) -> None:
        evidence_root = self.root / "evidence"
        evidence_root.mkdir(mode=0o700)
        with mock.patch.object(
            self.module,
            "DEFAULT_EVIDENCE_ROOT",
            evidence_root,
        ):
            attempt = self.module._default_evidence_directory()
        self.assertEqual(attempt.parent, evidence_root)
        self.assertFalse(attempt.exists())

    def test_sudo_fresh_root_owned_shared_parent_couples_lock_owner(
        self,
    ) -> None:
        account = self.root.stat()
        shared_lock = self.root / "codex" / "heavy.lock"
        original_fstat = self.module.os.fstat

        def root_owned_directories(descriptor: int) -> object:
            metadata = original_fstat(descriptor)
            if stat.S_ISDIR(metadata.st_mode):
                return SimpleNamespace(
                    st_mode=metadata.st_mode,
                    st_uid=0,
                    st_gid=0,
                )
            return metadata

        sudo_environment = {
            "SUDO_UID": str(account.st_uid),
            "SUDO_GID": str(account.st_gid),
        }
        with (
            mock.patch.object(self.module.os, "geteuid", return_value=0),
            mock.patch.object(self.module.os, "getegid", return_value=0),
            mock.patch.object(self.module.os, "environ", sudo_environment),
            mock.patch.object(
                self.module.os,
                "fstat",
                side_effect=root_owned_directories,
            ),
            mock.patch.object(self.module.os, "fchown") as fchown,
            mock.patch.object(
                self.module.os,
                "fchmod",
                wraps=self.module.os.fchmod,
            ) as fchmod,
        ):
            descriptor = self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=account.st_uid,
                shared_gid=account.st_gid,
            )
        os.close(descriptor)
        parent_metadata = shared_lock.parent.stat()
        self.assertEqual(
            stat.S_IMODE(parent_metadata.st_mode),
            0o1777,
        )
        self.assertTrue(shared_lock.is_file())
        fchown.assert_called_once_with(
            descriptor,
            0,
            account.st_gid,
        )
        self.assertIn(
            (descriptor, 0o660),
            [call.args for call in fchmod.call_args_list],
        )

    def test_sudo_existing_invoking_owned_parent_couples_lock_owner(
        self,
    ) -> None:
        account = self.root.stat()
        self.root.chmod(0o1777)
        shared_lock = self.root / "heavy.lock"
        sudo_environment = {
            "SUDO_UID": str(account.st_uid),
            "SUDO_GID": str(account.st_gid),
        }
        with (
            mock.patch.object(self.module.os, "geteuid", return_value=0),
            mock.patch.object(self.module.os, "getegid", return_value=0),
            mock.patch.object(self.module.os, "environ", sudo_environment),
            mock.patch.object(
                self.module.os,
                "fchown",
            ) as fchown,
            mock.patch.object(
                self.module.os,
                "fchmod",
            ) as fchmod,
        ):
            descriptor = self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=account.st_uid,
                shared_gid=account.st_gid,
            )
        os.close(descriptor)
        fchown.assert_called_once_with(
            descriptor,
            account.st_uid,
            account.st_gid,
        )
        fchmod.assert_called_once_with(descriptor, 0o660)

    def test_shared_parent_creation_race_binds_lock_to_winner_owner(
        self,
    ) -> None:
        account = self.root.stat()
        shared_parent = self.root / "codex"
        shared_lock = shared_parent / "heavy.lock"
        original_mkdir = self.module.os.mkdir

        def concurrent_winner(
            path: str,
            mode: int,
            *,
            dir_fd: int,
        ) -> None:
            original_mkdir(path, mode, dir_fd=dir_fd)
            shared_parent.chmod(0o1777)
            raise FileExistsError

        with (
            mock.patch.object(
                self.module.os,
                "mkdir",
                side_effect=concurrent_winner,
            ),
            mock.patch.object(
                self.module.os,
                "fchown",
            ) as fchown,
        ):
            descriptor = self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=account.st_uid,
                shared_gid=account.st_gid,
            )
        os.close(descriptor)
        self.assertEqual(
            stat.S_IMODE(shared_parent.stat().st_mode),
            0o1777,
        )
        fchown.assert_not_called()
        lock_metadata = shared_lock.stat()
        self.assertEqual(
            (lock_metadata.st_uid, lock_metadata.st_gid),
            (account.st_uid, account.st_gid),
        )

    def test_shared_lock_rejects_untrusted_existing_parent_before_chown(
        self,
    ) -> None:
        account = self.root.stat()
        shared_lock = self.root / "heavy.lock"
        original_fstat = self.module.os.fstat

        def foreign_parent(descriptor: int) -> object:
            metadata = original_fstat(descriptor)
            if stat.S_ISDIR(metadata.st_mode):
                return SimpleNamespace(
                    st_mode=metadata.st_mode,
                    st_uid=account.st_uid + 1,
                    st_gid=account.st_gid,
                )
            return metadata

        with (
            mock.patch.object(
                self.module.os,
                "fstat",
                side_effect=foreign_parent,
            ),
            mock.patch.object(self.module.os, "fchown") as fchown,
            self.assertRaisesRegex(
                self.module.QualificationError,
                "parent is untrusted",
            ),
        ):
            self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=account.st_uid,
                shared_gid=account.st_gid,
            )
        fchown.assert_not_called()

    def test_shared_lock_rejects_multi_link_inode_before_normalization(
        self,
    ) -> None:
        account = self.root.stat()
        source = self.root / "user-data"
        source.write_text("must not be normalized", encoding="utf-8")
        shared_lock = self.root / "heavy.lock"
        os.link(source, shared_lock)
        with (
            mock.patch.object(self.module.os, "fchown") as fchown,
            mock.patch.object(self.module.os, "fchmod") as fchmod,
            self.assertRaisesRegex(
                self.module.QualificationError,
                "qualification lock is untrusted",
            ),
        ):
            self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=account.st_uid,
                shared_gid=account.st_gid,
            )
        fchown.assert_not_called()
        fchmod.assert_not_called()
        self.assertEqual(
            source.read_text(encoding="utf-8"),
            "must not be normalized",
        )

    def test_private_session_lock_rejects_multi_link_inode_before_normalization(
        self,
    ) -> None:
        source = self.root / "user-data"
        source.write_text("must remain private", encoding="utf-8")
        session_lock = self.root / "session.lock"
        os.link(source, session_lock)
        with (
            mock.patch.object(self.module.os, "fchown") as fchown,
            mock.patch.object(self.module.os, "fchmod") as fchmod,
            self.assertRaisesRegex(
                self.module.QualificationError,
                "qualification lock is untrusted",
            ),
        ):
            self.module._safe_open_lock(session_lock)
        fchown.assert_not_called()
        fchmod.assert_not_called()
        self.assertEqual(
            source.read_text(encoding="utf-8"),
            "must remain private",
        )

    def test_sudo_repairs_root_0644_lock_for_parent_owner_path_flock(
        self,
    ) -> None:
        account = self.root.stat()
        shared_lock = self.root / "heavy.lock"
        shared_lock.touch(mode=0o644)
        sudo_environment = {
            "SUDO_UID": str(account.st_uid),
            "SUDO_GID": str(account.st_gid),
        }
        with (
            mock.patch.object(self.module.os, "geteuid", return_value=0),
            mock.patch.object(self.module.os, "getegid", return_value=0),
            mock.patch.object(self.module.os, "environ", sudo_environment),
            mock.patch.object(self.module.os, "fchown") as fchown,
            mock.patch.object(self.module.os, "fchmod") as fchmod,
        ):
            descriptor = self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=account.st_uid,
                shared_gid=account.st_gid,
            )
        try:
            fchown.assert_called_once_with(
                descriptor,
                account.st_uid,
                account.st_gid,
            )
            fchmod.assert_called_once_with(descriptor, 0o660)
        finally:
            os.close(descriptor)

    def test_nonroot_accepts_invoking_owned_group_lock_readwrite(
        self,
    ) -> None:
        shared_lock = self.root / "heavy.lock"
        shared_lock.touch(mode=0o660)
        original_fstat = self.module.os.fstat

        def synthetic_invoking_metadata(descriptor: int) -> object:
            actual = original_fstat(descriptor)
            if stat.S_ISDIR(actual.st_mode):
                return SimpleNamespace(
                    st_mode=actual.st_mode,
                    st_uid=12345,
                    st_gid=12345,
                )
            return SimpleNamespace(
                st_mode=stat.S_IFREG | 0o660,
                st_uid=12345,
                st_gid=12345,
                st_size=actual.st_size,
                st_nlink=1,
            )

        with (
            mock.patch.object(self.module.os, "geteuid", return_value=12345),
            mock.patch.object(self.module.os, "getegid", return_value=12345),
            mock.patch.object(
                self.module.os,
                "open",
                wraps=self.module.os.open,
            ) as open_file,
            mock.patch.object(
                self.module.os,
                "fstat",
                side_effect=synthetic_invoking_metadata,
            ),
            mock.patch.object(self.module.os, "fchmod") as fchmod,
            mock.patch.object(self.module.os, "fchown") as fchown,
        ):
            descriptor = self.module._safe_open_lock(
                shared_lock,
                shared_global=True,
                shared_uid=12345,
                shared_gid=12345,
            )
        try:
            lock_opens = [
                call
                for call in open_file.call_args_list
                if call.args and call.args[0] == shared_lock.name
            ]
            self.assertEqual(len(lock_opens), 1)
            self.assertEqual(
                lock_opens[0].args[1] & os.O_ACCMODE,
                os.O_RDWR,
            )
            fchmod.assert_called_once_with(descriptor, 0o660)
            fchown.assert_not_called()
        finally:
            os.close(descriptor)

    def test_shared_lock_rejects_unvalidated_owner_or_group(self) -> None:
        current_uid = os.geteuid()
        current_gid = os.getegid()
        cases = (
            (current_uid + 1, current_gid),
            (current_uid, current_gid + 1),
        )
        for index, (shared_uid, shared_gid) in enumerate(cases):
            with (
                self.subTest(shared_uid=shared_uid, shared_gid=shared_gid),
                self.assertRaisesRegex(
                    self.module.QualificationError,
                    "shared qualification lock",
                ),
            ):
                self.module._safe_open_lock(
                    self.root / f"forged-{index}.lock",
                    shared_global=True,
                    shared_uid=shared_uid,
                    shared_gid=shared_gid,
                )

    def test_sudo_private_session_lock_becomes_root_private(self) -> None:
        account = self.root.stat()
        session_lock = self.root / "session.lock"
        session_lock.touch(mode=0o644)
        sudo_environment = {
            "SUDO_UID": str(account.st_uid),
            "SUDO_GID": str(account.st_gid),
        }
        with (
            mock.patch.object(self.module.os, "geteuid", return_value=0),
            mock.patch.object(self.module.os, "getegid", return_value=0),
            mock.patch.object(self.module.os, "environ", sudo_environment),
            mock.patch.object(self.module.os, "fchown") as fchown,
            mock.patch.object(self.module.os, "fchmod") as fchmod,
        ):
            descriptor = self.module._safe_open_lock(session_lock)
        try:
            fchown.assert_called_once_with(descriptor, 0, 0)
            fchmod.assert_called_once_with(descriptor, 0o600)
        finally:
            os.close(descriptor)

    def test_util_linux_path_flock_permission_failure_and_group_repair(
        self,
    ) -> None:
        shared_lock = self.root / "flock-path.lock"
        shared_lock.touch(mode=0o000)
        denied = subprocess.run(
            ["/usr/bin/flock", "--nonblock", str(shared_lock), "true"],
            capture_output=True,
            check=False,
            timeout=2,
        )
        self.assertEqual(denied.returncode, 66)
        self.assertIn(b"Permission denied", denied.stderr)
        shared_lock.chmod(0o660)
        admitted = subprocess.run(
            ["/usr/bin/flock", "--nonblock", str(shared_lock), "true"],
            capture_output=True,
            check=False,
            timeout=2,
        )
        self.assertEqual(admitted.returncode, 0, admitted.stderr)

    def test_lock_and_evidence_parent_symlinks_are_rejected(self) -> None:
        target = self.root / "target"
        target.mkdir(mode=0o700)
        redirected = self.root / "redirected"
        redirected.symlink_to(target, target_is_directory=True)
        with self.assertRaises(self.module.QualificationError):
            self.module._safe_open_lock(redirected / "lock")
        with self.assertRaises(self.module.QualificationError):
            self.module._create_evidence_directory(redirected / "evidence")
        self.assertFalse((target / "lock").exists())
        self.assertFalse((target / "evidence").exists())

    def test_default_evidence_root_can_be_created_directly_under_sticky_tmp(
        self,
    ) -> None:
        evidence_root = Path(
            tempfile.mkdtemp(prefix="xenoteer-phase6-root-test-", dir="/tmp")
        )
        evidence_root.rmdir()
        try:
            with mock.patch.object(
                self.module,
                "DEFAULT_EVIDENCE_ROOT",
                evidence_root,
            ):
                attempt = self.module._default_evidence_directory()
            self.assertEqual(attempt.parent, evidence_root)
            self.assertEqual(stat.S_IMODE(evidence_root.stat().st_mode), 0o700)
        finally:
            if evidence_root.exists():
                evidence_root.rmdir()

    def test_identity_and_docker_probes_use_only_trusted_system_path(self) -> None:
        docker_output = json.dumps(
            [
                {
                    "Id": PRODUCTION_ID,
                    "Config": {"Labels": {}},
                    "RootFS": {"Layers": []},
                },
                {
                    "Id": FIXTURE_ID,
                    "Config": {"Labels": {}},
                    "RootFS": {"Layers": []},
                },
            ]
        ).encode("utf-8")
        with mock.patch.object(
            self.module.subprocess,
            "run",
            side_effect=(
                SimpleNamespace(returncode=0, stdout=b"identity\n"),
                SimpleNamespace(returncode=0, stdout=docker_output),
            ),
        ) as run:
            self.module._run_source_command(
                ("/usr/bin/git", "status"),
                REPOSITORY_ROOT,
            )
            self.module.inspect_exact_images(PRODUCTION_ID, FIXTURE_ID)
        self.assertEqual(run.call_count, 2)
        for invocation in run.call_args_list:
            with self.subTest(command=invocation.args[0]):
                self.assertEqual(
                    invocation.kwargs["env"]["PATH"],
                    self.module.TRUSTED_SYSTEM_PATH,
                )
                self.assertNotIn("/tmp", invocation.kwargs["env"]["PATH"])

    def test_root_probe_preserves_only_validated_sudo_checkout_identity(
        self,
    ) -> None:
        repository_metadata = REPOSITORY_ROOT.stat()
        supplied = {
            "SUDO_UID": str(repository_metadata.st_uid),
            "SUDO_GID": str(repository_metadata.st_gid),
            "HOME": "/tmp/attacker",
            "LD_PRELOAD": "/tmp/attacker.so",
            "PATH": "/tmp/attacker",
        }
        with (
            mock.patch.object(self.module.os, "geteuid", return_value=0),
            mock.patch.object(self.module.os, "environ", supplied),
        ):
            environment = self.module._trusted_source_environment(REPOSITORY_ROOT)
        self.assertEqual(environment["SUDO_UID"], supplied["SUDO_UID"])
        self.assertEqual(environment["SUDO_GID"], supplied["SUDO_GID"])
        self.assertEqual(environment["PATH"], self.module.TRUSTED_SYSTEM_PATH)
        self.assertNotIn("HOME", environment)
        self.assertNotIn("LD_PRELOAD", environment)

    def test_root_probe_rejects_malformed_or_forged_sudo_identity(self) -> None:
        repository_metadata = REPOSITORY_ROOT.stat()
        cases = (
            {
                "SUDO_UID": "not-a-uid",
                "SUDO_GID": str(repository_metadata.st_gid),
            },
            {
                "SUDO_UID": str(repository_metadata.st_uid + 1),
                "SUDO_GID": str(repository_metadata.st_gid),
            },
            {
                "SUDO_UID": str(repository_metadata.st_uid),
                "SUDO_GID": str(repository_metadata.st_gid + 1),
            },
        )
        for supplied in cases:
            with (
                self.subTest(supplied=supplied),
                mock.patch.object(self.module.os, "geteuid", return_value=0),
                mock.patch.object(self.module.os, "environ", supplied),
                self.assertRaisesRegex(
                    self.module.QualificationError,
                    "sudo checkout identity",
                ),
            ):
                self.module._trusted_source_environment(REPOSITORY_ROOT)


class ImportSafetyContractTests(unittest.TestCase):
    def test_import_has_no_subprocess_signal_temporary_or_privilege_side_effects(
        self,
    ) -> None:
        specification = importlib.util.spec_from_file_location(
            "xenoteer_phase6_qualification_import_safety",
            MODULE_PATH,
        )
        assert specification is not None
        assert specification.loader is not None
        imported = importlib.util.module_from_spec(specification)
        with (
            mock.patch("subprocess.run") as run,
            mock.patch("subprocess.Popen") as popen,
            mock.patch("signal.signal") as install_signal,
            mock.patch("tempfile.TemporaryDirectory") as temporary,
            mock.patch("os.chown") as chown,
            mock.patch("os.setuid") as setuid,
            mock.patch("os.setgid") as setgid,
        ):
            sys.modules[specification.name] = imported
            try:
                specification.loader.exec_module(imported)
            finally:
                sys.modules.pop(specification.name, None)
        run.assert_not_called()
        popen.assert_not_called()
        install_signal.assert_not_called()
        temporary.assert_not_called()
        chown.assert_not_called()
        setuid.assert_not_called()
        setgid.assert_not_called()

    def test_cli_requires_root_before_any_preflight_side_effect(self) -> None:
        module = require_module()
        stderr = io.StringIO()
        with (
            mock.patch.object(os, "geteuid", return_value=1000),
            mock.patch.object(module, "qualify") as qualify,
            mock.patch.object(
                module,
                "_default_evidence_directory",
            ) as evidence,
            mock.patch("sys.stderr", stderr),
            self.assertRaisesRegex(SystemExit, "64"),
        ):
            module.main([PRODUCTION_ID, FIXTURE_ID])
        qualify.assert_not_called()
        evidence.assert_not_called()
        self.assertIn("must run as root through sudo", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
