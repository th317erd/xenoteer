#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Canonical, serial, exact-image Phase 6 release qualification."""

from __future__ import annotations

import dataclasses
import enum
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import select
import signal
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from typing import NoReturn


SDK_SCRIPTS = Path(__file__).resolve().parents[1] / "sdk"
if str(SDK_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SDK_SCRIPTS))

from qualification_identity import (  # noqa: E402
    GateError as IdentityError,
    SourceIdentity as CheckoutSourceIdentity,
    current_dependency_lock_digest,
    current_source_identity as read_checkout_source_identity,
    read_fixture_artifact_lock,
    reject_daemon_overrides as reject_identity_overrides,
    validate_exact_image_ids,
    validate_release_image_metadata,
)


IMAGE_ID = re.compile(r"sha256:[0-9a-f]{64}\Z")
PLAIN_SHA256 = re.compile(r"[0-9a-f]{64}\Z")
HEAD_REVISION = re.compile(r"[0-9a-f]{40}\Z")
QUALIFICATION_LOCK = Path("/tmp/codex/xenoteer-phase6-qualification.lock")
HEAVY_BUILD_LOCK = Path("/tmp/codex/xenoteer-heavy-build.lock")
DEFAULT_EVIDENCE_ROOT = Path("/tmp/xenoteer-phase6-qualification-evidence")
NICE = Path("/usr/bin/nice")
IONICE = Path("/usr/bin/ionice")
DOCKER = Path("/usr/bin/docker")
GIT = Path("/usr/bin/git")
TRUSTED_SYSTEM_PATH = "/usr/sbin:/usr/bin:/sbin:/bin"
TRUSTED_PROBE_ENVIRONMENT = {
    "LANG": "C",
    "LC_ALL": "C",
    "PATH": TRUSTED_SYSTEM_PATH,
}
MAX_LANE_LOG_BYTES = 64 * 1024 * 1024
QUICKSTART_SUMMARY_PREFIX = "public quick-start qualification passed: "
QUICKSTART_KEYS = frozenset(
    {
        "fixture_image",
        "npm",
        "production_image",
        "python_sdist",
        "python_wheel",
        "rust_protocol",
        "rust_sdk",
        "source_tree",
    }
)
class QualificationError(RuntimeError):
    """One fail-closed release-qualification error."""


@dataclasses.dataclass(frozen=True)
class ImageMetadata:
    """Immutable image identity and the metadata used for admission."""

    image_id: str
    labels: Mapping[str, str]
    layers: tuple[str, ...]
    runtime_configuration: Mapping[str, object] = dataclasses.field(
        default_factory=dict
    )


@dataclasses.dataclass(frozen=True)
class SourceIdentity:
    """Current clean checkout and dependency-lock identity."""

    revision: str
    source_tree_sha256: str
    dependency_lock_sha256: str
    clean: bool


@dataclasses.dataclass(frozen=True)
class ExactImagePair:
    """One admitted production image and its exact derived fixture."""

    production: ImageMetadata
    fixture: ImageMetadata
    source: SourceIdentity


class LockMode(enum.Enum):
    """Whether the orchestrator or the lane owns the heavy-build lock."""

    OUTER = "outer"
    INNER = "inner"


@dataclasses.dataclass(frozen=True)
class Lane:
    """One immutable Phase 6 qualification lane."""

    name: str
    image_id: str
    command: tuple[str, ...]
    environment: tuple[tuple[str, str], ...]
    lock_mode: LockMode
    timeout_seconds: float
    priority: tuple[int, int]
    cleanup_grace_seconds: float = 15.0


def validate_image_ids(production_id: str, fixture_id: str) -> None:
    """Require two already-resolved, distinct, lowercase image IDs."""

    try:
        validate_exact_image_ids(production_id, fixture_id)
    except IdentityError as error:
        raise QualificationError(str(error)) from error


def reject_daemon_overrides(environment: Mapping[str, str]) -> None:
    """Reject every diagnostic daemon substitution, including empty values."""

    try:
        reject_identity_overrides(environment)
    except IdentityError as error:
        raise QualificationError(str(error)) from error


def _validated_invoking_identity(
    repository_root: Path,
    environment: Mapping[str, str],
) -> tuple[int, int]:
    effective_uid = os.geteuid()
    if effective_uid != 0:
        record = pwd.getpwuid(effective_uid)
        return record.pw_uid, record.pw_gid
    repository_metadata = repository_root.stat()
    sudo_uid = environment.get("SUDO_UID")
    sudo_gid = environment.get("SUDO_GID")
    if sudo_uid is None and sudo_gid is None and repository_metadata.st_uid == 0:
        return 0, 0
    numeric_identity = re.compile(r"(?:0|[1-9][0-9]{0,9})\Z")
    if (
        sudo_uid is None
        or sudo_gid is None
        or numeric_identity.fullmatch(sudo_uid) is None
        or numeric_identity.fullmatch(sudo_gid) is None
        or int(sudo_uid) != repository_metadata.st_uid
    ):
        raise QualificationError("sudo checkout identity is malformed or forged")
    try:
        account = pwd.getpwuid(int(sudo_uid))
    except KeyError as error:
        raise QualificationError("sudo checkout identity has no local account") from error
    if int(sudo_gid) != account.pw_gid:
        raise QualificationError("sudo checkout identity is malformed or forged")
    return account.pw_uid, account.pw_gid


def _lane_environment(
    repository_root: Path,
    source_environment: Mapping[str, str],
) -> dict[str, str]:
    """Construct the reviewed child environment; never forward caller knobs."""

    invoking_uid, invoking_gid = _validated_invoking_identity(
        repository_root,
        source_environment,
    )
    try:
        invoking_account = pwd.getpwuid(invoking_uid)
        effective_account = pwd.getpwuid(os.geteuid())
    except KeyError as error:
        raise QualificationError("invoking build identity has no local account") from error
    if (
        invoking_account.pw_gid != invoking_gid
        or not effective_account.pw_dir.startswith("/")
    ):
        raise QualificationError("invoking build identity is inconsistent")
    home = Path(effective_account.pw_dir)
    environment = {
        "HOME": str(home),
        "LANG": "C",
        "LC_ALL": "C",
        "LOGNAME": effective_account.pw_name,
        "PATH": TRUSTED_SYSTEM_PATH,
        "PYTHONDONTWRITEBYTECODE": "1",
        "USER": effective_account.pw_name,
        "XENOTEER_DESKTOP_MATRIX_SCOPE": "full",
    }
    if os.geteuid() == 0 and invoking_uid != 0:
        environment["SUDO_UID"] = str(invoking_uid)
        environment["SUDO_GID"] = str(invoking_gid)
    return environment


def _package_tool_path(
    repository_root: Path,
    source_environment: Mapping[str, str],
) -> str:
    invoking_uid, _ = _validated_invoking_identity(
        repository_root,
        source_environment,
    )
    try:
        invoking_home = pwd.getpwuid(invoking_uid).pw_dir
    except KeyError as error:
        raise QualificationError("invoking build identity has no local account") from error
    return f"{invoking_home}/.cargo/bin:{TRUSTED_SYSTEM_PATH}"


def qualification_lanes(
    repository_root: Path,
    python_executable: str,
    pair: ExactImagePair,
    package_tool_path: str = TRUSTED_SYSTEM_PATH,
) -> tuple[Lane, ...]:
    """Return the frozen seven-lane release qualification table."""

    python = Path(python_executable)
    if not python.is_absolute():
        raise QualificationError("Phase 6 Python executable must be absolute")
    container = repository_root / "scripts/container"
    sdk = repository_root / "scripts/sdk"
    production = pair.production.image_id
    fixture = pair.fixture.image_id
    return (
        Lane(
            "phase5-atspi-live",
            fixture,
            (str(python), str(container / "test-phase5-atspi-live.py"), fixture),
            (),
            LockMode.OUTER,
            25 * 60,
            (15, 3),
            45.0,
        ),
        Lane(
            "production-lifecycle",
            production,
            (str(container / "test-image.sh"), production),
            (),
            LockMode.INNER,
            35 * 60,
            (15, 3),
            45.0,
        ),
        Lane(
            "phase4-live-fixtures",
            fixture,
            (str(python), str(container / "test-phase4-live-fixtures.py"), fixture),
            (),
            LockMode.OUTER,
            20 * 60,
            (15, 3),
            45.0,
        ),
        Lane(
            "phase4-event-flood",
            production,
            (str(container / "test-phase4-event-flood.sh"), production),
            (),
            LockMode.INNER,
            10 * 60,
            (15, 3),
            45.0,
        ),
        Lane(
            "novnc",
            production,
            (str(container / "test-novnc-spike.sh"),),
            (("XENOTEER_NOVNC_SPIKE_BASE_IMAGE", production),),
            LockMode.OUTER,
            10 * 60,
            (15, 3),
            45.0,
        ),
        Lane(
            "desktop-app-matrix",
            fixture,
            (str(container / "test-desktop-app-image.sh"), fixture),
            (),
            LockMode.OUTER,
            25 * 60,
            (15, 3),
            45.0,
        ),
        Lane(
            "public-quickstarts",
            fixture,
            (str(python), str(sdk / "test-public-quickstarts.py"), fixture),
            (("PATH", package_tool_path),),
            LockMode.INNER,
            20 * 60,
            (15, 3),
            45.0,
        ),
    )


def _trusted_host_identities() -> tuple[set[int], set[int]]:
    allowed_uids = {0, os.geteuid()}
    allowed_gids = {0, os.getegid()}
    if os.geteuid() == 0:
        sudo_uid = os.environ.get("SUDO_UID")
        sudo_gid = os.environ.get("SUDO_GID")
        if (
            sudo_uid is not None
            and sudo_gid is not None
            and re.fullmatch(r"(?:0|[1-9][0-9]{0,9})", sudo_uid)
            and re.fullmatch(r"(?:0|[1-9][0-9]{0,9})", sudo_gid)
        ):
            try:
                account = pwd.getpwuid(int(sudo_uid))
            except KeyError:
                pass
            else:
                if account.pw_gid == int(sudo_gid):
                    allowed_uids.add(account.pw_uid)
                    allowed_gids.add(account.pw_gid)
    return allowed_uids, allowed_gids


def _open_trusted_parent(path: Path, *, description: str) -> int:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_DIRECTORY", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise QualificationError(f"{description} parent is unavailable") from error
    metadata = os.fstat(descriptor)
    allowed_uids, allowed_gids = _trusted_host_identities()
    mode = stat.S_IMODE(metadata.st_mode)
    trusted_writers = (
        not mode & 0o002
        or bool(metadata.st_mode & stat.S_ISVTX)
    ) and (
        not mode & 0o020
        or metadata.st_gid in allowed_gids
        or bool(metadata.st_mode & stat.S_ISVTX)
    )
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid not in allowed_uids
        or not trusted_writers
    ):
        os.close(descriptor)
        raise QualificationError(f"{description} parent is untrusted")
    return descriptor


def _ensure_shared_lock_parent(
    path: Path,
    *,
    create_missing: bool,
) -> tuple[int, int]:
    """Open one trusted lock parent and return its fd-bound owner."""

    if not create_missing:
        descriptor = _open_trusted_parent(
            path,
            description="qualification lock",
        )
        return descriptor, os.fstat(descriptor).st_uid
    grandparent_descriptor = _open_trusted_parent(
        path.parent,
        description="shared lock",
    )
    created = False
    descriptor: int | None = None
    try:
        try:
            os.mkdir(path.name, mode=0o1777, dir_fd=grandparent_descriptor)
            created = True
        except FileExistsError:
            pass
        flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
        flags |= getattr(os, "O_DIRECTORY", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(
            path.name,
            flags,
            dir_fd=grandparent_descriptor,
        )
        if created:
            os.fchmod(descriptor, 0o1777)
        metadata = os.fstat(descriptor)
        allowed_uids, allowed_gids = _trusted_host_identities()
        mode = stat.S_IMODE(metadata.st_mode)
        trusted_writers = (
            not mode & 0o002
            or bool(metadata.st_mode & stat.S_ISVTX)
        ) and (
            not mode & 0o020
            or metadata.st_gid in allowed_gids
            or bool(metadata.st_mode & stat.S_ISVTX)
        )
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid not in allowed_uids
            or not trusted_writers
            or (
                path == HEAVY_BUILD_LOCK.parent
                and mode != 0o1777
            )
        ):
            os.close(descriptor)
            descriptor = None
            raise QualificationError("shared lock parent is untrusted")
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        raise QualificationError("shared lock parent is unavailable") from error
    finally:
        os.close(grandparent_descriptor)
    assert descriptor is not None
    return descriptor, metadata.st_uid


def _safe_open_lock(
    path: Path,
    *,
    shared_global: bool = False,
    shared_uid: int | None = None,
    shared_gid: int | None = None,
) -> int:
    base_flags = getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    allowed_uids, allowed_gids = _trusted_host_identities()
    if shared_global:
        effective_shared_uid = os.geteuid() if shared_uid is None else shared_uid
        effective_shared_gid = os.getegid() if shared_gid is None else shared_gid
        if (
            isinstance(effective_shared_uid, bool)
            or not isinstance(effective_shared_uid, int)
            or effective_shared_uid < 0
            or effective_shared_uid > 4_294_967_295
            or effective_shared_uid not in allowed_uids
            or isinstance(effective_shared_gid, bool)
            or not isinstance(effective_shared_gid, int)
            or effective_shared_gid < 0
            or effective_shared_gid > 4_294_967_295
            or effective_shared_gid not in allowed_gids
        ):
            raise QualificationError("shared qualification lock group is untrusted")
    else:
        effective_shared_uid = None
        effective_shared_gid = None
    parent_descriptor, parent_uid = _ensure_shared_lock_parent(
        path.parent,
        create_missing=shared_global,
    )
    if shared_global:
        effective_lock_uid = parent_uid
    else:
        effective_lock_uid = None
    created = False
    try:
        if shared_global:
            try:
                descriptor = os.open(
                    path.name,
                    os.O_RDWR | base_flags,
                    dir_fd=parent_descriptor,
                )
            except FileNotFoundError:
                try:
                    descriptor = os.open(
                        path.name,
                        os.O_CREAT | os.O_EXCL | os.O_RDWR | base_flags,
                        0o660,
                        dir_fd=parent_descriptor,
                    )
                    created = True
                except FileExistsError:
                    descriptor = os.open(
                        path.name,
                        os.O_RDWR | base_flags,
                        dir_fd=parent_descriptor,
                    )
        else:
            descriptor = os.open(
                path.name,
                os.O_CREAT | os.O_RDWR | base_flags,
                0o600,
                dir_fd=parent_descriptor,
            )
    except OSError as error:
        raise QualificationError(f"could not open qualification lock: {path}") from error
    finally:
        os.close(parent_descriptor)
    metadata = os.fstat(descriptor)
    mode = stat.S_IMODE(metadata.st_mode)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_nlink != 1
        or metadata.st_uid not in allowed_uids
        or bool(mode & 0o002)
        or bool(mode & 0o020) and metadata.st_gid not in allowed_gids
    ):
        os.close(descriptor)
        raise QualificationError(f"qualification lock is untrusted: {path}")
    try:
        if shared_global:
            assert effective_shared_gid is not None
            assert effective_shared_uid is not None
            assert effective_lock_uid is not None
            if os.geteuid() == 0:
                os.fchown(
                    descriptor,
                    effective_lock_uid,
                    effective_shared_gid,
                )
                os.fchmod(descriptor, 0o660)
            elif created or metadata.st_uid == os.geteuid():
                if effective_lock_uid != os.geteuid():
                    raise QualificationError(
                        "non-root cannot create a parent-owned shared lock"
                    )
                if metadata.st_gid != effective_shared_gid:
                    os.fchown(descriptor, -1, effective_shared_gid)
                os.fchmod(descriptor, 0o660)
            elif (
                metadata.st_uid != effective_lock_uid
                or metadata.st_gid != effective_shared_gid
                or stat.S_IMODE(metadata.st_mode) != 0o660
            ):
                raise QualificationError(
                    f"foreign shared qualification lock is untrusted: {path}"
                )
        else:
            if os.geteuid() == 0:
                os.fchown(descriptor, 0, 0)
            os.fchmod(descriptor, 0o600)
    except QualificationError:
        os.close(descriptor)
        raise
    except OSError as error:
        os.close(descriptor)
        raise QualificationError(
            f"could not normalize qualification lock: {path}"
        ) from error
    return descriptor


@contextmanager
def exclusive_lock(
    path: Path,
    *,
    description: str,
    wait_seconds: float | None,
    shared_global: bool = False,
    shared_uid: int | None = None,
    shared_gid: int | None = None,
) -> Iterator[None]:
    """Hold one safe advisory lock, optionally with a bounded wait."""

    descriptor = _safe_open_lock(
        path,
        shared_global=shared_global,
        shared_uid=shared_uid,
        shared_gid=shared_gid,
    )
    deadline = None if wait_seconds is None else time.monotonic() + wait_seconds
    try:
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError as error:
                if deadline is None or time.monotonic() >= deadline:
                    raise QualificationError(description) from error
                time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
        yield
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)


def probe_heavy_build_lock(
    path: Path,
    *,
    shared_uid: int,
    shared_gid: int,
) -> None:
    """Prove no caller has incorrectly enclosed the complete qualification."""

    with exclusive_lock(
        path,
        description=(
            "heavy-build lock is already held; invoke the canonical Phase 6 "
            "runner directly without an outer heavy-build lock"
        ),
        wait_seconds=None,
        shared_global=True,
        shared_uid=shared_uid,
        shared_gid=shared_gid,
    ):
        pass


def _validate_image_metadata(
    production: ImageMetadata,
    fixture: ImageMetadata,
    source: SourceIdentity,
    repository_root: Path,
) -> ExactImagePair:
    validate_image_ids(production.image_id, fixture.image_id)
    if not source.clean:
        raise QualificationError("current source checkout is dirty")
    if (
        HEAD_REVISION.fullmatch(source.revision) is None
        or PLAIN_SHA256.fullmatch(source.source_tree_sha256) is None
        or PLAIN_SHA256.fullmatch(source.dependency_lock_sha256) is None
    ):
        raise QualificationError("current source identity is malformed")
    inspected = json.dumps(
        [
            {
                "Id": production.image_id,
                "Config": {
                    **production.runtime_configuration,
                    "Labels": dict(production.labels),
                },
                "RootFS": {"Layers": list(production.layers)},
            },
            {
                "Id": fixture.image_id,
                "Config": {
                    **fixture.runtime_configuration,
                    "Labels": dict(fixture.labels),
                },
                "RootFS": {"Layers": list(fixture.layers)},
            },
        ],
        sort_keys=True,
    )
    checkout = CheckoutSourceIdentity(
        head_revision=source.revision,
        source_tree_sha256=source.source_tree_sha256,
        dirty=False,
        revision=source.revision,
    )
    try:
        fixture_lock = read_fixture_artifact_lock(
            repository_root / "container/fixtures/desktop-apps/artifacts.lock"
        )
        validate_release_image_metadata(
            production.image_id,
            fixture.image_id,
            inspected,
            checkout,
            source.dependency_lock_sha256 + "\n",
            fixture_lock,
        )
    except IdentityError as error:
        raise QualificationError(str(error)) from error
    return ExactImagePair(production, fixture, source)


def _metadata_from_inspect(value: object) -> ImageMetadata:
    if not isinstance(value, dict):
        raise QualificationError("Docker returned malformed image metadata")
    image_id = value.get("Id")
    configuration = value.get("Config")
    rootfs = value.get("RootFS")
    if (
        not isinstance(image_id, str)
        or not isinstance(configuration, dict)
        or not isinstance(rootfs, dict)
    ):
        raise QualificationError("Docker omitted required image metadata")
    labels = configuration.get("Labels")
    layers = rootfs.get("Layers")
    if not isinstance(labels, dict) or not isinstance(layers, list):
        raise QualificationError("Docker omitted image labels or layers")
    runtime_configuration = dict(configuration)
    runtime_configuration.pop("Labels", None)
    return ImageMetadata(
        image_id,
        labels,
        tuple(layers),
        runtime_configuration,
    )


def inspect_exact_images(
    production_id: str,
    fixture_id: str,
) -> tuple[ImageMetadata, ImageMetadata]:
    """Inspect both exact IDs once, in the order supplied."""

    validate_image_ids(production_id, fixture_id)
    try:
        completed = subprocess.run(
            [str(DOCKER), "image", "inspect", production_id, fixture_id],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            check=False,
            env=TRUSTED_PROBE_ENVIRONMENT,
            timeout=20,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as error:
        raise QualificationError("bounded Docker image inspection failed") from error
    if completed.returncode != 0:
        raise QualificationError("Docker could not inspect both exact image IDs")
    try:
        values = json.loads(completed.stdout)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise QualificationError("Docker returned malformed image inspection JSON") from error
    if not isinstance(values, list) or len(values) != 2:
        raise QualificationError("Docker did not return exactly two image records")
    production = _metadata_from_inspect(values[0])
    fixture = _metadata_from_inspect(values[1])
    if production.image_id != production_id or fixture.image_id != fixture_id:
        raise QualificationError("Docker image inspection changed identity or order")
    return production, fixture


def _hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _validated_lane_log_hash(log_path: Path, lane_name: str) -> str:
    try:
        metadata = log_path.lstat()
    except OSError as error:
        raise QualificationError(f"{lane_name} lane log is unavailable") from error
    if (
        not stat.S_ISREG(metadata.st_mode)
        or log_path.is_symlink()
        or metadata.st_size > MAX_LANE_LOG_BYTES
    ):
        raise QualificationError(
            f"{lane_name} lane log exceeds the evidence limit or is unsafe"
        )
    return _hash_file(log_path)


def _read_lane_log(log_path: Path, lane_name: str) -> str:
    _validated_lane_log_hash(log_path, lane_name)
    with log_path.open("rb") as source:
        encoded = source.read(MAX_LANE_LOG_BYTES + 1)
    if len(encoded) > MAX_LANE_LOG_BYTES:
        raise QualificationError(
            f"{lane_name} lane log exceeds the evidence limit"
        )
    try:
        return encoded.decode("utf-8")
    except UnicodeDecodeError as error:
        raise QualificationError(f"{lane_name} lane log is not UTF-8") from error


def _run_source_command(
    command: Sequence[str],
    repository_root: Path,
    *,
    binary: bool = False,
) -> str | bytes:
    try:
        completed = subprocess.run(
            list(command),
            cwd=repository_root,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            check=False,
            env=_trusted_source_environment(repository_root),
            timeout=10,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as error:
        raise QualificationError("source identity command failed") from error
    if completed.returncode != 0:
        raise QualificationError("source identity command returned nonzero")
    if binary:
        return completed.stdout
    try:
        return completed.stdout.decode("utf-8")
    except UnicodeDecodeError as error:
        raise QualificationError("source identity command returned non-UTF-8") from error


def _trusted_source_environment(repository_root: Path) -> dict[str, str]:
    """Build a minimal Git/tool environment with validated sudo ownership."""

    environment = dict(TRUSTED_PROBE_ENVIRONMENT)
    invoking_uid, invoking_gid = _validated_invoking_identity(
        repository_root,
        os.environ,
    )
    if os.geteuid() == 0 and invoking_uid != 0:
        environment["SUDO_UID"] = str(invoking_uid)
        environment["SUDO_GID"] = str(invoking_gid)
    return environment


@dataclasses.dataclass(frozen=True)
class _TextResult:
    stdout: str


class _IdentityExecutor:
    """Bounded absolute-command adapter for the shared identity module."""

    @staticmethod
    def _absolute(command: Sequence[str]) -> list[str]:
        values = list(command)
        if values and values[0] == "git":
            values[0] = str(GIT)
        return values

    def run(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> _TextResult:
        if timeout <= 0 or timeout > 10 or cwd is None:
            raise QualificationError("identity command has an invalid execution bound")
        output = _run_source_command(self._absolute(command), cwd)
        assert isinstance(output, str)
        return _TextResult(output)

    def run_bytes(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> bytes:
        if timeout <= 0 or timeout > 10 or cwd is None:
            raise QualificationError("identity command has an invalid execution bound")
        output = _run_source_command(
            self._absolute(command),
            cwd,
            binary=True,
        )
        assert isinstance(output, bytes)
        return output


def current_source_identity(repository_root: Path) -> SourceIdentity:
    """Read the structured source identity through the shared pure contract."""

    executor = _IdentityExecutor()
    try:
        checkout = read_checkout_source_identity(repository_root, executor)
        dependency_lock_sha256 = current_dependency_lock_digest(
            repository_root,
            executor,
        )
    except IdentityError as error:
        raise QualificationError(str(error)) from error
    return SourceIdentity(
        revision=checkout.revision,
        source_tree_sha256=checkout.source_tree_sha256,
        dependency_lock_sha256=dependency_lock_sha256,
        clean=not checkout.dirty,
    )


def _atomic_json(path: Path, value: Mapping[str, object], *, exclusive: bool) -> None:
    encoded = (
        json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    temporary = path.parent / f".{path.name}.{os.getpid()}.{secrets.token_hex(6)}.tmp"
    descriptor = os.open(
        temporary,
        os.O_CREAT | os.O_EXCL | os.O_WRONLY | getattr(os, "O_CLOEXEC", 0),
        0o600,
    )
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as destination:
            destination.write(encoded)
            destination.flush()
            os.fsync(destination.fileno())
    finally:
        os.close(descriptor)
    try:
        if exclusive:
            try:
                os.link(temporary, path)
            except FileExistsError as error:
                raise QualificationError(f"evidence already exists: {path}") from error
            finally:
                temporary.unlink(missing_ok=True)
        else:
            os.replace(temporary, path)
        os.chmod(path, 0o600)
    finally:
        temporary.unlink(missing_ok=True)


def _fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    flags |= getattr(os, "O_DIRECTORY", 0)
    flags |= getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _create_evidence_directory(path: Path) -> None:
    parent_descriptor = _open_trusted_parent(
        path.parent,
        description="evidence directory",
    )
    try:
        os.mkdir(path.name, mode=0o700, dir_fd=parent_descriptor)
        metadata = os.stat(
            path.name,
            dir_fd=parent_descriptor,
            follow_symlinks=False,
        )
    except FileExistsError as error:
        raise QualificationError(f"evidence directory already exists: {path}") from error
    except OSError as error:
        raise QualificationError(f"could not create evidence directory: {path}") from error
    finally:
        os.close(parent_descriptor)
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o700
    ):
        raise QualificationError("created evidence directory is untrusted")


def _terminate_process_group(
    process: subprocess.Popen[bytes],
    grace_seconds: float,
) -> None:
    process.poll()
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + grace_seconds
    while time.monotonic() < deadline:
        process.poll()
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            return
        time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    process.wait(timeout=max(1.0, grace_seconds))


def _wait_process_group_exit(process_group: int, wait_seconds: float) -> bool:
    deadline = time.monotonic() + wait_seconds
    while True:
        try:
            os.killpg(process_group, 0)
        except ProcessLookupError:
            return True
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        time.sleep(min(0.05, remaining))


def run_lane_process(
    lane: Lane,
    log_path: Path,
    *,
    cwd: Path,
    environment: Mapping[str, str],
    terminate_grace_seconds: float | None = None,
) -> int:
    """Run one lane in a bounded process group with private combined output."""

    cleanup_grace = (
        lane.cleanup_grace_seconds
        if terminate_grace_seconds is None
        else terminate_grace_seconds
    )
    if lane.timeout_seconds <= 0 or cleanup_grace <= 0:
        raise QualificationError(f"{lane.name} has an invalid process deadline")
    descriptor = os.open(
        log_path,
        os.O_CREAT | os.O_EXCL | os.O_WRONLY | getattr(os, "O_CLOEXEC", 0),
        0o600,
    )
    executable = Path(lane.command[0]) if lane.command else Path()
    try:
        if (
            not lane.command
            or not executable.is_absolute()
            or (executable.is_symlink() and not executable.exists())
            or not executable.is_file()
            or not os.access(executable, os.X_OK)
        ):
            raise QualificationError(f"{lane.name} required command is unavailable")
        command = (
            str(NICE),
            "-n",
            str(lane.priority[0]),
            str(IONICE),
            "-c",
            str(lane.priority[1]),
            *lane.command,
        )
        child_environment = dict(environment)
        child_environment.update(lane.environment)
        child_environment["PYTHONDONTWRITEBYTECODE"] = "1"
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=child_environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except FileNotFoundError as error:
            raise QualificationError(
                f"{lane.name} required command is unavailable"
            ) from error
        assert process.stdout is not None
        output_descriptor = process.stdout.fileno()
        written = 0
        deadline = time.monotonic() + lane.timeout_seconds
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    _terminate_process_group(process, cleanup_grace)
                    raise QualificationError(f"{lane.name} timed out")
                ready, _, _ = select.select(
                    [output_descriptor],
                    [],
                    [],
                    min(0.1, remaining),
                )
                if not ready:
                    continue
                chunk = os.read(output_descriptor, 64 * 1024)
                if not chunk:
                    exit_status = process.wait(timeout=max(0.1, remaining))
                    if _wait_process_group_exit(process.pid, cleanup_grace):
                        return exit_status
                    _terminate_process_group(process, cleanup_grace)
                    raise QualificationError(
                        f"{lane.name} left descendant processes after exit"
                    )
                allowed = MAX_LANE_LOG_BYTES - written
                if allowed > 0:
                    view = memoryview(chunk[:allowed])
                    while view:
                        consumed = os.write(descriptor, view)
                        view = view[consumed:]
                    written += min(len(chunk), allowed)
                if len(chunk) > allowed:
                    _terminate_process_group(process, cleanup_grace)
                    raise QualificationError(
                        f"{lane.name} exceeded the lane evidence limit"
                    )
        except BaseException:
            _terminate_process_group(process, cleanup_grace)
            raise
        finally:
            process.stdout.close()
    finally:
        os.close(descriptor)


def parse_quickstart_summary(
    output: str,
    pair: ExactImagePair,
) -> dict[str, str]:
    """Parse the sole final, identity-bound package qualification summary."""

    lines = [line for line in output.splitlines() if line]
    matches = [line for line in lines if line.startswith(QUICKSTART_SUMMARY_PREFIX)]
    if len(matches) != 1 or not lines or matches[0] != lines[-1]:
        raise QualificationError("public-quickstarts summary is missing or duplicated")
    summary = matches[0]
    if re.search(r"(?i)(authorization|bearer|secret|token)", summary):
        raise QualificationError("public-quickstarts summary contains secret-bearing text")
    fields: dict[str, str] = {}
    for field in summary.removeprefix(QUICKSTART_SUMMARY_PREFIX).split():
        key, separator, value = field.partition("=")
        if not separator or not key or not value or key in fields:
            raise QualificationError("public-quickstarts summary is malformed")
        fields[key] = value
    if set(fields) != QUICKSTART_KEYS:
        raise QualificationError("public-quickstarts summary fields are incomplete")
    if (
        fields["production_image"] != pair.production.image_id
        or fields["fixture_image"] != pair.fixture.image_id
        or fields["source_tree"] != pair.source.source_tree_sha256
    ):
        raise QualificationError("public-quickstarts summary identities do not match")
    for key in QUICKSTART_KEYS - {
        "production_image",
        "fixture_image",
        "source_tree",
    }:
        if IMAGE_ID.fullmatch(fields[key]) is None:
            raise QualificationError("public-quickstarts artifact digest is malformed")
    return fields


def _write_rejection(
    attempt_path: Path,
    attempt: dict[str, object],
    lane: Lane,
    *,
    exit_status: int | str,
) -> None:
    attempt["status"] = "rejected"
    attempt["failed_lane"] = lane.name
    attempt["exit_status"] = exit_status
    _atomic_json(attempt_path, attempt, exclusive=False)


def _ensure_failure_log(log_path: Path) -> str | None:
    """Create a private, non-secret diagnostic log when a runner produced none."""

    try:
        descriptor = os.open(
            log_path,
            os.O_CREAT | os.O_EXCL | os.O_WRONLY | getattr(os, "O_CLOEXEC", 0),
            0o600,
        )
    except FileExistsError:
        try:
            metadata = log_path.lstat()
        except FileNotFoundError:
            return None
        if not stat.S_ISREG(metadata.st_mode):
            return None
    else:
        try:
            os.write(
                descriptor,
                b"lane runner failed before producing complete evidence\n",
            )
        finally:
            os.close(descriptor)
    try:
        os.chmod(log_path, 0o600)
    except OSError:
        return None
    try:
        metadata = log_path.lstat()
    except OSError:
        return None
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > MAX_LANE_LOG_BYTES:
        return None
    return _hash_file(log_path)


def _record_runner_failure(
    attempt_path: Path,
    attempt: dict[str, object],
    lane: Lane,
    log_path: Path,
    *,
    started: float,
    exit_status: str,
) -> None:
    try:
        log_sha256 = _ensure_failure_log(log_path)
    except Exception:
        log_sha256 = None
    lane_record = {
        "name": lane.name,
        "status": "rejected",
        "exit_status": exit_status,
        "duration_milliseconds": max(
            0,
            int((time.monotonic() - started) * 1000),
        ),
        "log_sha256": log_sha256,
    }
    cast_lanes = attempt["lanes"]
    assert isinstance(cast_lanes, list)
    cast_lanes.append(lane_record)
    _write_rejection(
        attempt_path,
        attempt,
        lane,
        exit_status=exit_status,
    )


def _record_post_lane_failure(
    attempt_path: Path,
    attempt: dict[str, object],
    lane: Lane,
    log_path: Path,
    *,
    started: float,
    exit_status: str,
) -> None:
    try:
        log_sha256 = _ensure_failure_log(log_path)
    except Exception:
        log_sha256 = None
    cast_lanes = attempt["lanes"]
    assert isinstance(cast_lanes, list)
    if cast_lanes and cast_lanes[-1].get("name") == lane.name:
        lane_record = cast_lanes[-1]
        lane_record["status"] = "rejected"
        lane_record["exit_status"] = exit_status
        if lane_record.get("log_sha256") is None:
            lane_record["log_sha256"] = log_sha256
    else:
        cast_lanes.append(
            {
                "name": lane.name,
                "status": "rejected",
                "exit_status": exit_status,
                "duration_milliseconds": max(
                    0,
                    int((time.monotonic() - started) * 1000),
                ),
                "log_sha256": log_sha256,
            }
        )
    _write_rejection(
        attempt_path,
        attempt,
        lane,
        exit_status=exit_status,
    )


def _json_sha256(value: Mapping[str, object]) -> str:
    encoded = (
        json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def qualify(
    production_id: str,
    fixture_id: str,
    evidence_directory: Path,
    *,
    repository_root: Path,
    python_executable: str,
    heavy_lock_path: Path = HEAVY_BUILD_LOCK,
    session_lock_path: Path = QUALIFICATION_LOCK,
    inspect_image: Callable[
        [str, str], tuple[ImageMetadata, ImageMetadata]
    ] = inspect_exact_images,
    source_probe: Callable[[], SourceIdentity] | None = None,
    lane_runner: Callable[[Lane, Path], int] | None = None,
    environment: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Run all seven lanes exactly once and return identities after full success."""

    validate_image_ids(production_id, fixture_id)
    source_environment = dict(os.environ if environment is None else environment)
    reject_daemon_overrides(source_environment)
    invoking_uid, invoking_gid = _validated_invoking_identity(
        repository_root,
        source_environment,
    )
    child_environment = _lane_environment(repository_root, source_environment)
    effective_source_probe = (
        (lambda: current_source_identity(repository_root))
        if source_probe is None
        else source_probe
    )
    if lane_runner is None:
        effective_lane_runner = lambda lane, log: run_lane_process(
            lane,
            log,
            cwd=repository_root,
            environment=child_environment,
        )
    else:
        effective_lane_runner = lane_runner
    if session_lock_path.parent == HEAVY_BUILD_LOCK.parent:
        shared_parent_descriptor, _ = _ensure_shared_lock_parent(
            session_lock_path.parent,
            create_missing=True,
        )
        os.close(shared_parent_descriptor)
    with exclusive_lock(
        session_lock_path,
        description="another Phase 6 qualification is already running",
        wait_seconds=None,
    ):
        probe_heavy_build_lock(
            heavy_lock_path,
            shared_uid=invoking_uid,
            shared_gid=invoking_gid,
        )
        initial_source = effective_source_probe()
        production, fixture = inspect_image(production_id, fixture_id)
        pair = _validate_image_metadata(
            production,
            fixture,
            initial_source,
            repository_root,
        )
        if effective_source_probe() != initial_source:
            raise QualificationError("source identity changed before lane 1")
        lanes = qualification_lanes(
            repository_root,
            python_executable,
            pair,
            package_tool_path=_package_tool_path(
                repository_root,
                source_environment,
            ),
        )
        _create_evidence_directory(evidence_directory)
        attempt_path = evidence_directory / "attempt.json"
        attempt: dict[str, object] = {
            "schema": "xenoteer-phase6-qualification/v1",
            "status": "running",
            "production_image": production_id,
            "fixture_image": fixture_id,
            "source_tree": initial_source.source_tree_sha256,
            "revision": initial_source.revision,
            "dependency_lock": initial_source.dependency_lock_sha256,
            "lanes": [],
        }
        _atomic_json(attempt_path, attempt, exclusive=False)
        summary: dict[str, str] | None = None
        for lane_index, lane in enumerate(lanes, start=1):
            log_path = evidence_directory / f"lane-{lane_index:02d}-{lane.name}.log"
            started = time.monotonic()
            try:
                if lane.lock_mode is LockMode.OUTER:
                    with exclusive_lock(
                        heavy_lock_path,
                        description=f"{lane.name} could not acquire the heavy-build lock",
                        wait_seconds=120,
                        shared_global=True,
                        shared_uid=invoking_uid,
                        shared_gid=invoking_gid,
                    ):
                        exit_status = effective_lane_runner(lane, log_path)
                else:
                    exit_status = effective_lane_runner(lane, log_path)
            except BaseException as error:
                failure_status = (
                    "runner-error"
                    if isinstance(error, Exception)
                    else "interrupted"
                )
                _record_runner_failure(
                    attempt_path,
                    attempt,
                    lane,
                    log_path,
                    started=started,
                    exit_status=failure_status,
                )
                if isinstance(error, QualificationError):
                    raise
                if isinstance(error, Exception):
                    raise QualificationError(
                        f"{lane.name} runner failed"
                    ) from error
                raise
            try:
                if log_path.exists():
                    os.chmod(log_path, 0o600)
                duration_milliseconds = max(
                    0,
                    int((time.monotonic() - started) * 1000),
                )
                log_sha256 = (
                    _validated_lane_log_hash(log_path, lane.name)
                    if log_path.is_file()
                    else None
                )
                lane_record = {
                    "name": lane.name,
                    "status": "passed" if exit_status == 0 else "rejected",
                    "exit_status": exit_status,
                    "duration_milliseconds": duration_milliseconds,
                    "log_sha256": log_sha256,
                }
                cast_lanes = attempt["lanes"]
                assert isinstance(cast_lanes, list)
                cast_lanes.append(lane_record)
                if exit_status != 0 or log_sha256 is None:
                    _write_rejection(
                        attempt_path,
                        attempt,
                        lane,
                        exit_status=exit_status,
                    )
                    raise QualificationError(
                        f"{lane.name} rejected Phase 6 qualification "
                        f"with status {exit_status}"
                    )
                if lane.name == "public-quickstarts":
                    try:
                        summary = parse_quickstart_summary(
                            _read_lane_log(log_path, lane.name),
                            pair,
                        )
                    except UnicodeDecodeError as error:
                        raise QualificationError(
                            "public-quickstarts log is not UTF-8"
                        ) from error
                if effective_source_probe() != initial_source:
                    raise QualificationError(
                        f"source identity changed after {lane.name}"
                    )
                _atomic_json(attempt_path, attempt, exclusive=False)
            except BaseException as error:
                if attempt.get("status") != "rejected":
                    failure_status = (
                        "evidence-error"
                        if isinstance(error, Exception)
                        else "interrupted"
                    )
                    _record_post_lane_failure(
                        attempt_path,
                        attempt,
                        lane,
                        log_path,
                        started=started,
                        exit_status=failure_status,
                    )
                if isinstance(error, QualificationError):
                    raise
                if isinstance(error, Exception):
                    raise QualificationError(
                        f"{lane.name} post-lane evidence failed"
                    ) from error
                raise
        qualification_path = evidence_directory / "qualification.json"
        try:
            if summary is None:
                raise QualificationError(
                    "public-quickstarts summary was never admitted"
                )
            result = {
                "production_image": production_id,
                "fixture_image": fixture_id,
                "source_tree": initial_source.source_tree_sha256,
                **{
                    key: summary[key]
                    for key in QUICKSTART_KEYS
                    if key
                    not in {"production_image", "fixture_image", "source_tree"}
                },
            }
            admitted_attempt = dict(attempt)
            admitted_attempt["status"] = "lanes-passed"
            _atomic_json(attempt_path, admitted_attempt, exclusive=False)
            _fsync_directory(evidence_directory)
            qualification = {
                "schema": "xenoteer-phase6-qualification/v1",
                "status": "passed",
                **result,
                "attempt_sha256": _json_sha256(admitted_attempt),
            }
            _atomic_json(
                qualification_path,
                qualification,
                exclusive=True,
            )
            _fsync_directory(evidence_directory)
            attempt.clear()
            attempt.update(admitted_attempt)
            return result
        except BaseException as error:
            qualification_path.unlink(missing_ok=True)
            final_lane = lanes[-1]
            final_log = evidence_directory / (
                f"lane-{len(lanes):02d}-{final_lane.name}.log"
            )
            if attempt.get("status") != "rejected":
                _record_post_lane_failure(
                    attempt_path,
                    attempt,
                    final_lane,
                    final_log,
                    started=time.monotonic(),
                    exit_status="final-evidence-error",
                )
            if isinstance(error, QualificationError):
                raise
            if isinstance(error, Exception):
                raise QualificationError(
                    "final qualification evidence failed"
                ) from error
            raise


def _signal_interrupt(signal_number: int, frame: object) -> NoReturn:
    del frame
    raise KeyboardInterrupt(f"interrupted by signal {signal_number}")


def _default_evidence_directory() -> Path:
    parent = DEFAULT_EVIDENCE_ROOT.parent
    try:
        parent_metadata = parent.lstat()
    except OSError as error:
        raise QualificationError("default evidence root parent is unavailable") from error
    parent_mode = stat.S_IMODE(parent_metadata.st_mode)
    if (
        not stat.S_ISDIR(parent_metadata.st_mode)
        or parent.is_symlink()
        or parent_metadata.st_uid not in {0, os.geteuid()}
        or (
            parent_mode & 0o022
            and not parent_metadata.st_mode & stat.S_ISVTX
        )
    ):
        raise QualificationError("default evidence root parent is untrusted")
    try:
        DEFAULT_EVIDENCE_ROOT.mkdir(mode=0o700, parents=False, exist_ok=False)
    except FileExistsError:
        pass
    except OSError as error:
        raise QualificationError("could not create default evidence root") from error
    try:
        root_metadata = DEFAULT_EVIDENCE_ROOT.lstat()
    except OSError as error:
        raise QualificationError("default evidence root is unavailable") from error
    if (
        not stat.S_ISDIR(root_metadata.st_mode)
        or DEFAULT_EVIDENCE_ROOT.is_symlink()
        or root_metadata.st_uid != os.geteuid()
        or stat.S_IMODE(root_metadata.st_mode) != 0o700
    ):
        raise QualificationError(
            "default evidence root must be an owned private directory"
        )
    return DEFAULT_EVIDENCE_ROOT / (
        f"attempt-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-"
        f"{os.getpid()}-{secrets.token_hex(6)}"
    )


def usage_error(message: str) -> NoReturn:
    print(message, file=sys.stderr)
    print(
        "usage: qualify-phase6.py PRODUCTION_SHA256_ID FIXTURE_SHA256_ID",
        file=sys.stderr,
    )
    raise SystemExit(64)


def main(arguments: Sequence[str] | None = None) -> int:
    """CLI entry point."""

    arguments = list(sys.argv[1:] if arguments is None else arguments)
    if len(arguments) != 2 or any(argument.startswith("-") for argument in arguments):
        usage_error("exactly two immutable image IDs are required")
    if os.geteuid() != 0:
        usage_error("canonical Phase 6 qualification must run as root through sudo")
    repository_root = Path(__file__).resolve().parents[2]
    previous_sigterm = signal.signal(signal.SIGTERM, _signal_interrupt)
    try:
        result = qualify(
            arguments[0],
            arguments[1],
            _default_evidence_directory(),
            repository_root=repository_root,
            python_executable=sys.executable,
        )
    except QualificationError as error:
        print(f"Phase 6 qualification failed: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Phase 6 qualification interrupted after cleanup", file=sys.stderr)
        return 130
    finally:
        signal.signal(signal.SIGTERM, previous_sigterm)
    print(
        "Phase 6 qualification passed: "
        + " ".join(f"{key}={result[key]}" for key in sorted(result))
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
