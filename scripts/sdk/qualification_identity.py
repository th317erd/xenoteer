#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Side-effect-free release identity validation shared by qualification gates."""

from __future__ import annotations

import dataclasses
import hashlib
import json
import re
import stat
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Protocol


DEFAULT_COMMAND_TIMEOUT_SECONDS = 10
IMAGE_ID = re.compile(r"sha256:[0-9a-f]{64}\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
HEAD_REVISION = re.compile(r"[0-9a-f]{40}\Z")
DAEMON_OVERRIDE_ENVIRONMENTS = (
    "XENOTEERD_BINARY_OVERRIDE",
    "XENOTEER_TEST_DAEMON_BINARY",
)
FIXTURE_DEBIAN_SNAPSHOT = "20260719T000000Z"
FIXTURE_ONLY_LABELS = frozenset(
    {
        "com.aeor.xenoteer.distribution-scope",
        "com.aeor.xenoteer.fixture",
        "com.aeor.xenoteer.fixture.debian-snapshot",
        "com.aeor.xenoteer.fixture.base-image-id",
        "com.aeor.xenoteer.fixture.electron-version",
        "com.aeor.xenoteer.fixture.electron-linux-x64-sha256",
    }
)
FIXTURE_ARTIFACT_LOCK_KEYS = frozenset(
    {
        "ELECTRON_VERSION",
        "ELECTRON_LINUX_X64_URL",
        "ELECTRON_LINUX_X64_SHA256",
    }
)


class GateError(RuntimeError):
    """One fail-closed release qualification error."""


class TextCommandResult(Protocol):
    stdout: str


class IdentityExecutor(Protocol):
    def run(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> TextCommandResult: ...

    def run_bytes(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> bytes: ...


@dataclasses.dataclass(frozen=True)
class UntrackedSource:
    """One untracked source identity in build-wrapper ordering."""

    path: str
    mode: str
    sha256: str


@dataclasses.dataclass(frozen=True)
class SourceIdentity:
    """The exact source labels produced by scripts/container/build.sh."""

    head_revision: str
    source_tree_sha256: str
    dirty: bool
    revision: str


@dataclasses.dataclass(frozen=True)
class FixtureArtifactLock:
    """The exact Electron artifact pinned by the desktop fixture."""

    electron_version: str
    electron_linux_x64_url: str
    electron_linux_x64_sha256: str


@dataclasses.dataclass(frozen=True)
class ExactFixtureImage:
    """Derived fixture plus the exact production image/source it extends."""

    fixture_id: str
    production_id: str
    source_tree_sha256: str
    fixture_debian_snapshot: str = ""
    electron_version: str = ""
    electron_linux_x64_sha256: str = ""
    revision: str = ""
    source_dirty: bool = False
    dependency_lock_sha256: str = ""


@dataclasses.dataclass(frozen=True)
class _ValidatedFixtureMetadata:
    image: ExactFixtureImage
    production_labels: Mapping[str, str]


def reject_daemon_overrides(environment: Mapping[str, str]) -> None:
    """Forbid diagnostic binary substitution in release qualification."""

    present = [
        variable for variable in DAEMON_OVERRIDE_ENVIRONMENTS if variable in environment
    ]
    if present:
        raise GateError(
            "public quick-start qualification rejects daemon override environment: "
            + ", ".join(present)
        )


def validate_image_id(value: str) -> str:
    """Return one immutable local Docker image ID or fail."""

    if IMAGE_ID.fullmatch(value) is None:
        raise GateError(f"image did not resolve to one immutable sha256 ID: {value!r}")
    return value


def validate_exact_image_ids(
    production_id: str,
    fixture_id: str,
) -> tuple[str, str]:
    """Require two distinct immutable IDs before image inspection."""

    production_id = validate_image_id(production_id)
    fixture_id = validate_image_id(fixture_id)
    if production_id == fixture_id:
        raise GateError("production and fixture image IDs must be distinct")
    return production_id, fixture_id


def source_snapshot_digest(
    head_revision: str,
    diff: bytes,
    untracked: Sequence[UntrackedSource],
) -> str:
    """Reproduce the exact source identity encoded by container/build.sh."""

    if HEAD_REVISION.fullmatch(head_revision) is None:
        raise GateError("git did not return one lowercase SHA-1 HEAD revision")
    digest = hashlib.sha256()
    digest.update(b"HEAD\0")
    digest.update(head_revision.encode("ascii"))
    digest.update(b"\0")
    digest.update(diff)
    for item in untracked:
        if (
            not item.path
            or "\0" in item.path
            or "\n" in item.path
            or "\t" in item.path
        ):
            raise GateError(f"unsupported untracked source path: {item.path!r}")
        if re.fullmatch(r"[0-7]{3,4}", item.mode) is None:
            raise GateError(f"invalid untracked source mode: {item.mode!r}")
        if SHA256.fullmatch(item.sha256) is None:
            raise GateError(f"invalid untracked source digest for {item.path!r}")
        digest.update(b"untracked\0")
        digest.update(item.path.encode("utf-8"))
        digest.update(b"\0")
        digest.update(item.mode.encode("ascii"))
        digest.update(b"\0")
        digest.update(item.sha256.encode("ascii"))
        digest.update(b"\0")
    return digest.hexdigest()


def file_sha256(path: Path) -> str:
    """Hash one regular, nonsymlinked file."""

    if path.is_symlink() or not path.is_file():
        raise GateError(f"expected one regular file: {path}")
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise GateError(f"could not hash required file: {path}") from error
    return digest.hexdigest()


def _read_current_source_snapshot(
    repository_root: Path,
    executor: IdentityExecutor,
) -> tuple[str, str]:
    head = executor.run(
        ["git", "rev-parse", "--verify", "HEAD"],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root,
    ).stdout.strip()
    if HEAD_REVISION.fullmatch(head) is None:
        raise GateError("git did not return one lowercase SHA-1 HEAD revision")
    diff = executor.run_bytes(
        ["git", "diff", "--binary", "--no-ext-diff", "HEAD", "--"],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root,
    )
    raw_paths = executor.run_bytes(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root,
    )
    paths = raw_paths.split(b"\0")
    if paths[-1] != b"":
        raise GateError("git untracked-file output was not NUL terminated")
    untracked: list[UntrackedSource] = []
    for raw_path in paths[:-1]:
        try:
            path_text = raw_path.decode("utf-8")
        except UnicodeDecodeError as error:
            raise GateError("untracked source path is not UTF-8") from error
        path = repository_root / path_text
        try:
            metadata = path.lstat()
        except OSError as error:
            raise GateError(
                f"could not inspect untracked source: {path_text!r}"
            ) from error
        if not stat.S_ISREG(metadata.st_mode):
            raise GateError(f"untracked source is not a regular file: {path_text!r}")
        untracked.append(
            UntrackedSource(
                path_text,
                format(stat.S_IMODE(metadata.st_mode), "o"),
                file_sha256(path),
            )
        )
    return head, source_snapshot_digest(head, diff, tuple(untracked))


def current_source_identity(
    repository_root: Path,
    executor: IdentityExecutor,
) -> SourceIdentity:
    """Read HEAD, hash, dirty state, and revision exactly like build.sh."""

    head, source_tree_sha256 = _read_current_source_snapshot(
        repository_root,
        executor,
    )
    dirty = bool(
        executor.run(
            [
                "git",
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ],
            timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            cwd=repository_root,
        ).stdout
    )
    revision = (
        f"{head}-dirty.{source_tree_sha256[:12]}"
        if dirty
        else head
    )
    return SourceIdentity(head, source_tree_sha256, dirty, revision)


def current_source_tree_hash(
    repository_root: Path,
    executor: IdentityExecutor,
) -> str:
    """Compatibility wrapper returning only the build-context digest."""

    _, source_tree_sha256 = _read_current_source_snapshot(repository_root, executor)
    return source_tree_sha256


def validate_dependency_lock_digest(output: str) -> str:
    """Validate the dependency-lock helper's single digest line."""

    match = re.fullmatch(r"([0-9a-f]{64})(?:\n)?", output)
    if match is None:
        raise GateError("dependency-lock helper returned an invalid SHA-256 digest")
    return match.group(1)


def current_dependency_lock_digest(
    repository_root: Path,
    executor: IdentityExecutor,
) -> str:
    """Run the canonical dependency-lock helper and validate its output."""

    helper = repository_root / "scripts" / "container" / "dependency-lock-hash.sh"
    if helper.is_symlink() or not helper.is_file():
        raise GateError("dependency-lock helper is missing or unsafe")
    result = executor.run(
        [str(helper)],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root,
    )
    return validate_dependency_lock_digest(result.stdout)


def parse_fixture_artifact_lock(contents: str) -> FixtureArtifactLock:
    """Parse exactly the three reviewed Electron fixture lock fields."""

    values: dict[str, str] = {}
    for line in contents.splitlines():
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if (
            not separator
            or not key
            or not value
            or key in values
            or key.strip() != key
            or value.strip() != value
        ):
            raise GateError("desktop fixture artifact lock is malformed")
        values[key] = value
    if set(values) != FIXTURE_ARTIFACT_LOCK_KEYS:
        raise GateError("desktop fixture artifact lock is malformed")
    version = values["ELECTRON_VERSION"]
    sha256 = values["ELECTRON_LINUX_X64_SHA256"]
    url = values["ELECTRON_LINUX_X64_URL"]
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version) is None:
        raise GateError("desktop fixture artifact lock has an invalid Electron version")
    if SHA256.fullmatch(sha256) is None:
        raise GateError("desktop fixture artifact lock has an invalid Electron digest")
    if re.fullmatch(r"https://[^\s]+", url) is None:
        raise GateError("desktop fixture artifact lock has an unexpected Electron URL")
    return FixtureArtifactLock(version, url, sha256)


def read_fixture_artifact_lock(path: Path) -> FixtureArtifactLock:
    """Read a regular fixture lock without following a repository symlink."""

    if path.is_symlink() or not path.is_file():
        raise GateError("desktop fixture artifact lock is missing or unsafe")
    try:
        contents = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise GateError("desktop fixture artifact lock could not be read") from error
    return parse_fixture_artifact_lock(contents)


def _validate_fixture_image_metadata(
    fixture_id: str,
    production_id: str,
    inspected_json: str,
) -> _ValidatedFixtureMetadata:
    production_id, fixture_id = validate_exact_image_ids(
        production_id,
        fixture_id,
    )
    try:
        values = json.loads(inspected_json)
    except json.JSONDecodeError as error:
        raise GateError("Docker returned malformed fixture image metadata") from error
    if (
        not isinstance(values, list)
        or len(values) != 2
        or not all(isinstance(value, dict) for value in values)
    ):
        raise GateError("Docker omitted exact production/fixture image metadata")
    production, fixture = values
    if production.get("Id") != production_id or fixture.get("Id") != fixture_id:
        raise GateError(
            "fixture or production image identity changed during inspection"
        )
    production_config = production.get("Config")
    fixture_config = fixture.get("Config")
    production_rootfs = production.get("RootFS")
    fixture_rootfs = fixture.get("RootFS")
    if not isinstance(production_config, dict) or not isinstance(
        fixture_config,
        dict,
    ):
        raise GateError("Docker omitted production or fixture runtime configuration")
    production_labels = production_config.get("Labels", {})
    fixture_labels = fixture_config.get("Labels", {})
    if (
        not isinstance(production_labels, dict)
        or not isinstance(fixture_labels, dict)
        or not all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in production_labels.items()
        )
        or not all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in fixture_labels.items()
        )
    ):
        raise GateError("Docker returned malformed production or fixture labels")
    if (
        set(fixture_labels) - set(production_labels) != FIXTURE_ONLY_LABELS
        or any(
            fixture_labels.get(key) != value
            for key, value in production_labels.items()
        )
    ):
        raise GateError("fixture changed inherited labels or added unknown labels")
    fixture_debian_snapshot = fixture_labels.get(
        "com.aeor.xenoteer.fixture.debian-snapshot"
    )
    electron_version = fixture_labels.get(
        "com.aeor.xenoteer.fixture.electron-version"
    )
    electron_linux_x64_sha256 = fixture_labels.get(
        "com.aeor.xenoteer.fixture.electron-linux-x64-sha256"
    )
    if (
        fixture_labels.get("com.aeor.xenoteer.distribution-scope")
        != "test-only-non-distributable"
        or fixture_labels.get("com.aeor.xenoteer.fixture")
        != "phase-2-desktop-apps"
        or fixture_labels.get("com.aeor.xenoteer.fixture.base-image-id")
        != production_id
        or fixture_debian_snapshot != FIXTURE_DEBIAN_SNAPSHOT
        or not isinstance(electron_version, str)
        or re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", electron_version) is None
        or not isinstance(electron_linux_x64_sha256, str)
        or SHA256.fullmatch(electron_linux_x64_sha256) is None
    ):
        raise GateError("image is not the exact recorded desktop fixture derivation")
    inherited_production_config = dict(production_config)
    inherited_fixture_config = dict(fixture_config)
    inherited_production_config.pop("Labels", None)
    inherited_fixture_config.pop("Labels", None)
    if inherited_fixture_config != inherited_production_config:
        raise GateError("fixture changed inherited Docker runtime configuration")
    source_tree_sha256 = production_labels.get(
        "com.aeor.xenoteer.source-tree.sha256"
    )
    if (
        not isinstance(source_tree_sha256, str)
        or SHA256.fullmatch(source_tree_sha256) is None
    ):
        raise GateError("production image omitted its exact source-tree hash label")
    if (
        fixture_labels.get("com.aeor.xenoteer.source-tree.sha256")
        != source_tree_sha256
    ):
        raise GateError("fixture image did not retain the production source identity")
    production_layers = (
        production_rootfs.get("Layers")
        if isinstance(production_rootfs, dict)
        else None
    )
    fixture_layers = (
        fixture_rootfs.get("Layers")
        if isinstance(fixture_rootfs, dict)
        else None
    )
    if (
        not isinstance(production_layers, list)
        or not production_layers
        or not all(isinstance(layer, str) and layer for layer in production_layers)
        or not isinstance(fixture_layers, list)
        or len(fixture_layers) <= len(production_layers)
        or fixture_layers[: len(production_layers)] != production_layers
    ):
        raise GateError(
            "fixture image does not preserve its exact production layer prefix"
        )
    image = ExactFixtureImage(
        fixture_id,
        production_id,
        source_tree_sha256,
        fixture_debian_snapshot,
        electron_version,
        electron_linux_x64_sha256,
    )
    return _ValidatedFixtureMetadata(image, production_labels)


def validate_fixture_image_metadata(
    fixture_id: str,
    production_id: str,
    inspected_json: str,
) -> ExactFixtureImage:
    """Preserve the fixture-first public quick-start metadata API."""

    return _validate_fixture_image_metadata(
        fixture_id,
        production_id,
        inspected_json,
    ).image


def _validate_source_identity(source: SourceIdentity) -> SourceIdentity:
    if (
        HEAD_REVISION.fullmatch(source.head_revision) is None
        or SHA256.fullmatch(source.source_tree_sha256) is None
        or not isinstance(source.dirty, bool)
    ):
        raise GateError("current checkout returned malformed source identity")
    expected_revision = (
        f"{source.head_revision}-dirty.{source.source_tree_sha256[:12]}"
        if source.dirty
        else source.head_revision
    )
    if source.revision != expected_revision:
        raise GateError("current checkout returned inconsistent source revision")
    return source


def validate_release_image_metadata(
    production_id: str,
    fixture_id: str,
    inspected_json: str,
    current_source: SourceIdentity,
    dependency_lock_output: str,
    fixture_lock: FixtureArtifactLock,
) -> ExactFixtureImage:
    """Bind one ordered inspect result to the clean checkout and lock inputs."""

    current_source = _validate_source_identity(current_source)
    if current_source.dirty:
        raise GateError("release qualification requires a clean source checkout")
    dependency_lock_sha256 = validate_dependency_lock_digest(dependency_lock_output)
    validated = _validate_fixture_image_metadata(
        fixture_id,
        production_id,
        inspected_json,
    )
    labels = validated.production_labels
    expected_labels = {
        "org.opencontainers.image.revision": current_source.revision,
        "com.aeor.xenoteer.source.dirty": "false",
        "com.aeor.xenoteer.source-tree.sha256": current_source.source_tree_sha256,
        "com.aeor.xenoteer.dependency-lock.sha256": dependency_lock_sha256,
    }
    if any(labels.get(key) != value for key, value in expected_labels.items()):
        raise GateError(
            "production image labels differ from the clean checkout identity"
        )
    image = validated.image
    if (
        image.electron_version != fixture_lock.electron_version
        or image.electron_linux_x64_sha256
        != fixture_lock.electron_linux_x64_sha256
    ):
        raise GateError("desktop fixture labels differ from the artifact lock")
    return dataclasses.replace(
        image,
        revision=current_source.revision,
        source_dirty=False,
        dependency_lock_sha256=dependency_lock_sha256,
    )
