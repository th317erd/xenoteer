#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Install staged public SDK artifacts and qualify them against one exact image."""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import pwd
import re
import secrets
import signal
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Mapping, Sequence
from pathlib import Path, PurePosixPath
from typing import Literal, NoReturn


DEFAULT_COMMAND_TIMEOUT_SECONDS = 10
PACKAGE_COMMAND_TIMEOUT_SECONDS = 120
QUICKSTART_COMMAND_TIMEOUT_SECONDS = 10
READINESS_TIMEOUT_SECONDS = 90
HEAVY_BUILD_LOCK = "/tmp/codex/xenoteer-heavy-build.lock"
IMAGE_ID = re.compile(r"sha256:[0-9a-f]{64}\Z")
LOOPBACK_PORT = re.compile(r"127\.0\.0\.1:([0-9]{1,5})\Z")
DAEMON_OVERRIDE_ENVIRONMENTS = (
    "XENOTEERD_BINARY_OVERRIDE",
    "XENOTEER_TEST_DAEMON_BINARY",
)


class GateError(RuntimeError):
    """One fail-closed public quick-start qualification error."""


@dataclasses.dataclass(frozen=True)
class CommandResult:
    """Captured bounded subprocess result."""

    returncode: int
    stdout: str
    stderr: str


@dataclasses.dataclass(frozen=True)
class UntrackedSource:
    """One untracked source identity in build-wrapper ordering."""

    path: str
    mode: str
    sha256: str


@dataclasses.dataclass(frozen=True)
class BuildIdentity:
    """Original non-root caller used for package assembly under sudo."""

    uid: int
    gid: int
    home: Path
    use_sudo: bool

    @classmethod
    def current(cls) -> "BuildIdentity":
        sudo_uid = os.environ.get("SUDO_UID")
        if os.geteuid() == 0 and sudo_uid not in (None, "", "0"):
            assert sudo_uid is not None
            try:
                uid = int(sudo_uid)
                record = pwd.getpwuid(uid)
            except (ValueError, KeyError) as error:
                raise GateError("SUDO_UID does not identify a local build user") from error
            sudo_gid = os.environ.get("SUDO_GID")
            try:
                gid = (
                    record.pw_gid
                    if sudo_gid is None or sudo_gid == ""
                    else int(sudo_gid)
                )
            except ValueError as error:
                raise GateError("SUDO_GID is not a numeric group identity") from error
            return cls(uid, gid, Path(record.pw_dir), True)
        record = pwd.getpwuid(os.geteuid())
        return cls(record.pw_uid, record.pw_gid, Path(record.pw_dir), False)

    def command(self, command: Sequence[str]) -> list[str]:
        low_priority = ["nice", "-n", "15", "ionice", "-c", "3", *command]
        if not self.use_sudo:
            return low_priority
        return ["sudo", "-H", "-u", f"#{self.uid}", "--", *low_priority]


class CommandExecutor:
    """Timeout-enforcing subprocess boundary."""

    def run(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        check: bool = True,
        cwd: Path | None = None,
        env: Mapping[str, str] | None = None,
    ) -> CommandResult:
        if timeout <= 0 or timeout > PACKAGE_COMMAND_TIMEOUT_SECONDS:
            raise GateError(f"invalid subprocess timeout: {timeout}")
        try:
            completed = subprocess.run(
                list(command),
                cwd=cwd,
                env=None if env is None else dict(env),
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
            )
        except FileNotFoundError as error:
            raise GateError(f"required command is unavailable: {command[0]}") from error
        except subprocess.TimeoutExpired as error:
            raise GateError(
                f"command exceeded its {timeout}s bound: {safe_command(command)}"
            ) from error
        result = CommandResult(completed.returncode, completed.stdout, completed.stderr)
        if check and result.returncode != 0:
            detail = safe_diagnostic(result.stderr or result.stdout)
            raise GateError(
                f"command failed ({safe_command(command)}): {detail or 'no safe diagnostic'}"
            )
        return result

    def run_bytes(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> bytes:
        if timeout <= 0 or timeout > DEFAULT_COMMAND_TIMEOUT_SECONDS:
            raise GateError(f"invalid binary subprocess timeout: {timeout}")
        try:
            completed = subprocess.run(
                list(command),
                cwd=cwd,
                capture_output=True,
                timeout=timeout,
                check=False,
            )
        except FileNotFoundError as error:
            raise GateError(f"required command is unavailable: {command[0]}") from error
        except subprocess.TimeoutExpired as error:
            raise GateError(
                f"command exceeded its {timeout}s bound: {safe_command(command)}"
            ) from error
        if completed.returncode != 0:
            raise GateError(f"command failed: {safe_command(command)}")
        return completed.stdout


@dataclasses.dataclass(frozen=True)
class PublicArtifacts:
    """Exact staged public archives and their source identity."""

    protocol_crate: Path
    sdk_crate: Path
    npm_tarball: Path
    python_wheel: Path
    python_sdist: Path
    source_tree_sha256: str

    def digests(self) -> dict[str, str]:
        return {
            "rust_protocol": file_sha256(self.protocol_crate),
            "rust_sdk": file_sha256(self.sdk_crate),
            "npm": file_sha256(self.npm_tarball),
            "python_wheel": file_sha256(self.python_wheel),
            "python_sdist": file_sha256(self.python_sdist),
        }


@dataclasses.dataclass(frozen=True)
class InstalledQuickstarts:
    """Commands and installed roots for every archive-derived consumer."""

    rust_command: tuple[str, ...]
    rust_root: Path
    typescript_command: tuple[str, ...]
    typescript_root: Path
    python_wheel_command: tuple[str, ...]
    python_wheel_root: Path
    python_sdist_command: tuple[str, ...]
    python_sdist_root: Path

    def variants(self) -> tuple[tuple[str, tuple[str, ...], Path], ...]:
        return (
            ("rust-crate", self.rust_command, self.rust_root),
            ("npm-tarball", self.typescript_command, self.typescript_root),
            ("python-wheel", self.python_wheel_command, self.python_wheel_root),
            ("python-sdist", self.python_sdist_command, self.python_sdist_root),
        )


class ContainerGuard:
    """Remove one known container on every exit path."""

    def __init__(
        self,
        executor: CommandExecutor,
        name: str,
        docker_command: Sequence[str] = ("docker",),
    ) -> None:
        self._executor = executor
        self._name = name
        self._docker_command = tuple(docker_command)
        self._created = False

    def __enter__(self) -> "ContainerGuard":
        return self

    def mark_created(self) -> None:
        self._created = True

    def cleanup(self) -> None:
        if not self._created:
            return
        result = self._executor.run(
            [
                *self._docker_command,
                "rm",
                "--force",
                "--volumes",
                self._name,
            ],
            timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            check=False,
        )
        self._created = False
        if result.returncode != 0:
            raise GateError("public quick-start container cleanup failed")

    def __exit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: object,
    ) -> Literal[False]:
        del exception_type, traceback
        try:
            self.cleanup()
        except GateError as cleanup_error:
            if exception is None:
                raise
            exception.add_note(str(cleanup_error))
        return False


def safe_command(command: Sequence[str]) -> str:
    """Render only argv; credentials are always passed through the environment."""

    return " ".join(str(argument) for argument in command)


def safe_diagnostic(value: str) -> str:
    """Bound subprocess prose and redact accidental authorization material."""

    redacted = re.sub(
        r"(?i)authorization\s*:\s*bearer\s+[A-Za-z0-9._~+/-]+=*",
        "Authorization: Bearer <redacted>",
        value,
    )
    redacted = redacted.strip()
    if len(redacted) <= 2_048:
        return redacted
    return redacted[:512] + "\n... diagnostic truncated ...\n" + redacted[-1_500:]


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


def parse_loopback_port(value: str) -> int:
    """Parse Docker's one expected dynamically published IPv4 binding."""

    match = LOOPBACK_PORT.fullmatch(value.strip())
    if match is None:
        raise GateError(f"Docker returned an unexpected API binding: {value!r}")
    port = int(match.group(1))
    if not 1 <= port <= 65_535:
        raise GateError("Docker returned an invalid dynamic API port")
    return port


def source_snapshot_digest(
    head_revision: str,
    diff: bytes,
    untracked: Sequence[UntrackedSource],
) -> str:
    """Reproduce the exact source identity encoded by container/build.sh."""

    digest = hashlib.sha256()
    digest.update(b"HEAD\0")
    digest.update(head_revision.encode("utf-8"))
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
        if re.fullmatch(r"[0-9a-f]{64}", item.sha256) is None:
            raise GateError(f"invalid untracked source digest for {item.path!r}")
        digest.update(b"untracked\0")
        digest.update(item.path.encode("utf-8"))
        digest.update(b"\0")
        digest.update(item.mode.encode("ascii"))
        digest.update(b"\0")
        digest.update(item.sha256.encode("ascii"))
        digest.update(b"\0")
    return digest.hexdigest()


def current_source_tree_hash(
    repository_root: Path,
    executor: CommandExecutor,
) -> str:
    """Read the current tree with the same bytes and ordering as the image wrapper."""

    head = executor.run(
        ["git", "rev-parse", "--verify", "HEAD"],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root,
    ).stdout.strip()
    if re.fullmatch(r"[0-9a-f]{40}", head) is None:
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
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise GateError(f"untracked source is not a regular file: {path_text!r}")
        untracked.append(
            UntrackedSource(
                path_text,
                format(stat.S_IMODE(metadata.st_mode), "o"),
                file_sha256(path),
            )
        )
    return source_snapshot_digest(head, diff, tuple(untracked))


def file_sha256(path: Path) -> str:
    """Hash one regular, nonsymlinked artifact."""

    if path.is_symlink() or not path.is_file():
        raise GateError(f"artifact is not a regular file: {path}")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _safe_archive_relative(member_name: str, expected_prefix: str) -> PurePosixPath:
    if "\0" in member_name or "\\" in member_name:
        raise GateError(f"crate contains an unsafe path: {member_name!r}")
    path = PurePosixPath(member_name)
    if (
        path.is_absolute()
        or len(path.parts) < 2
        or path.parts[0] != expected_prefix
        or any(part in ("", ".", "..") for part in path.parts)
        or path.as_posix() != member_name
    ):
        raise GateError(f"crate member escaped its package prefix: {member_name!r}")
    return PurePosixPath(*path.parts[1:])


def extract_crate_archive(
    archive: Path,
    destination: Path,
    *,
    expected_prefix: str,
) -> None:
    """Extract a validated Cargo archive without links or path traversal."""

    if destination.exists():
        raise GateError(f"crate extraction destination already exists: {destination}")
    temporary = destination.with_name(destination.name + ".extracting")
    if temporary.exists():
        raise GateError(f"crate extraction staging path already exists: {temporary}")
    temporary.mkdir(parents=True)
    seen: set[str] = set()
    try:
        with tarfile.open(archive, mode="r:*") as package:
            members = package.getmembers()
            if not members:
                raise GateError(f"crate archive is empty: {archive}")
            for member in members:
                if not member.isfile():
                    raise GateError(f"crate contains a non-file member: {member.name!r}")
                relative = _safe_archive_relative(member.name, expected_prefix)
                normalized = relative.as_posix()
                if normalized in seen:
                    raise GateError(f"crate contains a duplicate member: {normalized!r}")
                seen.add(normalized)
                source = package.extractfile(member)
                if source is None:
                    raise GateError(f"crate member is unreadable: {member.name!r}")
                target = temporary.joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open("xb") as output:
                    shutil.copyfileobj(source, output, length=1024 * 1024)
                target.chmod(member.mode & 0o777)
        temporary.rename(destination)
    except (OSError, tarfile.TarError) as error:
        raise GateError(f"could not safely extract crate {archive.name}: {error}") from error
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def _path_within(path: Path, root: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(root.resolve(strict=False))
    except ValueError:
        return False
    return True


def validate_cargo_artifact_origins(
    metadata_json: str,
    artifact_root: Path,
    repository_root: Path,
) -> None:
    """Require Cargo to resolve both Xenoteer crates from extracted archives."""

    try:
        metadata = json.loads(metadata_json)
    except json.JSONDecodeError as error:
        raise GateError("staged Rust consumer returned invalid Cargo metadata") from error
    packages = metadata.get("packages") if isinstance(metadata, dict) else None
    if not isinstance(packages, list):
        raise GateError("staged Rust consumer metadata omitted packages")
    for name in ("xenoteer-protocol", "xenoteer-sdk"):
        matches = [
            package
            for package in packages
            if isinstance(package, dict) and package.get("name") == name
        ]
        if len(matches) != 1:
            raise GateError(f"staged Rust consumer did not resolve exactly one {name}")
        manifest = matches[0].get("manifest_path")
        if not isinstance(manifest, str):
            raise GateError(f"staged Rust consumer omitted {name}'s manifest path")
        manifest_path = Path(manifest)
        if _path_within(manifest_path, repository_root):
            raise GateError(f"staged Rust consumer resolved {name} from the source tree")
        if not _path_within(manifest_path, artifact_root):
            raise GateError(f"staged Rust consumer resolved {name} outside staged artifacts")


def chown_tree(path: Path, identity: BuildIdentity) -> None:
    """Make a temporary tree writable by the original caller under sudo."""

    if not identity.use_sudo:
        return
    for current_root, directories, files in os.walk(path):
        os.chown(current_root, identity.uid, identity.gid)
        for name in directories:
            os.chown(Path(current_root) / name, identity.uid, identity.gid)
        for name in files:
            os.chown(Path(current_root) / name, identity.uid, identity.gid)


def _find_one(directory: Path, pattern: str, label: str) -> Path:
    candidates = sorted(directory.glob(pattern))
    if len(candidates) != 1:
        raise GateError(f"expected one staged {label}, found {len(candidates)}")
    candidate = candidates[0]
    file_sha256(candidate)
    return candidate


def parse_npm_pack_filename(output: str) -> str:
    """Accept npm 10's list or npm 12's package-keyed exact JSON inventory."""

    try:
        decoded = json.loads(output)
    except json.JSONDecodeError as error:
        raise GateError("npm pack did not return JSON") from error
    if isinstance(decoded, list):
        entries = decoded
    elif isinstance(decoded, dict) and set(decoded) == {"@xenoteer/sdk"}:
        entries = [decoded["@xenoteer/sdk"]]
    else:
        raise GateError("npm pack returned an unexpected artifact inventory")
    if (
        len(entries) != 1
        or not isinstance(entries[0], dict)
        or entries[0].get("name") != "@xenoteer/sdk"
    ):
        raise GateError("npm pack returned an unexpected SDK identity")
    filename = entries[0].get("filename")
    if (
        not isinstance(filename, str)
        or re.fullmatch(r"[A-Za-z0-9._-]+\.tgz", filename) is None
    ):
        raise GateError("npm pack returned an unsafe artifact filename")
    return filename


def _copy_python_source(source: Path, destination: Path) -> None:
    """Copy exact package inputs while excluding generated local build debris."""

    for path in source.rglob("*"):
        if path.is_symlink():
            raise GateError(f"Python package source contains a symlink: {path}")
    shutil.copytree(
        source,
        destination,
        ignore=shutil.ignore_patterns(
            "__pycache__",
            "*.pyc",
            "*.egg-info",
            "build",
            "dist",
        ),
    )


def stage_public_artifacts(
    repository_root: Path,
    workspace: Path,
    source_tree_sha256: str,
    executor: CommandExecutor,
    identity: BuildIdentity,
) -> PublicArtifacts:
    """Assemble every package archive without using one as a source import."""

    staging = workspace / "artifacts"
    cargo_target = staging / "cargo-target"
    npm_stage = staging / "npm"
    python_source = staging / "python-source"
    python_dist = staging / "python-dist"
    for directory in (cargo_target, npm_stage, python_dist):
        directory.mkdir(parents=True)
    _copy_python_source(repository_root / "packages" / "python", python_source)
    chown_tree(workspace, identity)

    cargo_binary = identity.home / ".cargo" / "bin" / "cargo"
    if not cargo_binary.is_file():
        resolved_cargo = shutil.which("cargo")
        if resolved_cargo is None:
            raise GateError("cargo is required to stage the Rust crates")
        cargo_binary = Path(resolved_cargo)
    for package in ("xenoteer-protocol", "xenoteer-sdk"):
        executor.run(
            identity.command(
                [
                    str(cargo_binary),
                    "package",
                    "--locked",
                    "--offline",
                    "--allow-dirty",
                    "--no-verify",
                    "--exclude-lockfile",
                    "--all-features",
                    "--target-dir",
                    str(cargo_target),
                    "--package",
                    package,
                ]
            ),
            timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
            cwd=repository_root,
        )
    protocol_crate = _find_one(
        cargo_target / "package",
        "xenoteer-protocol-*.crate",
        "protocol crate",
    )
    sdk_crate = _find_one(
        cargo_target / "package",
        "xenoteer-sdk-*.crate",
        "SDK crate",
    )

    executor.run(
        identity.command(["npm", "run", "build"]),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root / "packages" / "typescript",
    )
    npm_output = executor.run(
        identity.command(
            [
                "npm",
                "pack",
                "--json",
                "--pack-destination",
                str(npm_stage),
            ]
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root / "packages" / "typescript",
    ).stdout
    npm_tarball = npm_stage / parse_npm_pack_filename(npm_output)
    file_sha256(npm_tarball)

    executor.run(
        identity.command(
            [
                sys.executable,
                "-m",
                "build",
                "--no-isolation",
                "--outdir",
                str(python_dist),
                str(python_source),
            ]
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=workspace,
    )
    python_wheel = _find_one(
        python_dist,
        "xenoteer-*-py3-none-any.whl",
        "Python wheel",
    )
    python_sdist = _find_one(
        python_dist,
        "xenoteer-*.tar.gz",
        "Python source distribution",
    )
    executor.run(
        identity.command(
            [
                sys.executable,
                str(repository_root / "packages" / "python" / "scripts" / "verify_dist.py"),
                str(python_wheel),
                str(python_sdist),
            ]
        ),
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root,
    )

    return PublicArtifacts(
        protocol_crate,
        sdk_crate,
        npm_tarball,
        python_wheel,
        python_sdist,
        source_tree_sha256,
    )


def _crate_prefix(archive: Path) -> str:
    if not archive.name.endswith(".crate"):
        raise GateError(f"invalid Cargo archive name: {archive.name}")
    return archive.name.removesuffix(".crate")


def _write_text(path: Path, value: str, *, executable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding="utf-8")
    path.chmod(0o755 if executable else 0o644)


def prepare_installed_quickstarts(
    repository_root: Path,
    workspace: Path,
    artifacts: PublicArtifacts,
    executor: CommandExecutor,
    identity: BuildIdentity,
) -> InstalledQuickstarts:
    """Install every archive into isolated consumers and build their examples."""

    installs = workspace / "installed"
    installs.mkdir()

    rust_root = installs / "rust"
    rust_artifacts = rust_root / "archives"
    sdk_root = rust_artifacts / "xenoteer-sdk"
    protocol_root = rust_artifacts / "xenoteer-protocol"
    extract_crate_archive(
        artifacts.sdk_crate,
        sdk_root,
        expected_prefix=_crate_prefix(artifacts.sdk_crate),
    )
    extract_crate_archive(
        artifacts.protocol_crate,
        protocol_root,
        expected_prefix=_crate_prefix(artifacts.protocol_crate),
    )
    rust_consumer = rust_root / "consumer"
    _write_text(
        rust_consumer / "Cargo.toml",
        "[package]\n"
        'name = "xenoteer-public-quickstart"\n'
        'version = "0.0.0"\n'
        'edition = "2024"\n'
        "publish = false\n\n"
        "[dependencies]\n"
        f"xenoteer-sdk = {{ path = {json.dumps(str(sdk_root))} }}\n"
        'tokio = { version = "=1.53.1", features = ["macros", "rt-multi-thread", "time"] }\n\n'
        "[patch.crates-io]\n"
        f"xenoteer-protocol = {{ path = {json.dumps(str(protocol_root))} }}\n\n"
        "[workspace]\n",
    )
    (rust_consumer / "src").mkdir()
    shutil.copyfile(
        repository_root / "scripts" / "sdk" / "quickstarts" / "rust" / "main.rs",
        rust_consumer / "src" / "main.rs",
    )

    typescript_root = installs / "typescript"
    _write_text(
        typescript_root / "package.json",
        '{"name":"xenoteer-public-quickstart","private":true,"type":"module"}\n',
    )
    shutil.copyfile(
        repository_root
        / "scripts"
        / "sdk"
        / "quickstarts"
        / "typescript"
        / "quickstart.mjs",
        typescript_root / "quickstart.mjs",
    )

    python_source = (
        repository_root / "scripts" / "sdk" / "quickstarts" / "python" / "quickstart.py"
    )
    wheel_root = installs / "python-wheel"
    sdist_root = installs / "python-sdist"
    for root in (wheel_root, sdist_root):
        root.mkdir()
        shutil.copyfile(python_source, root / "quickstart.py")
    chown_tree(workspace, identity)

    cargo_binary = identity.home / ".cargo" / "bin" / "cargo"
    if not cargo_binary.is_file():
        resolved_cargo = shutil.which("cargo")
        if resolved_cargo is None:
            raise GateError("cargo is required to build the staged Rust consumer")
        cargo_binary = Path(resolved_cargo)
    cargo_environment = dict(os.environ)
    rust_target = repository_root / "target" / "phase6-public-quickstarts"
    cargo_environment.update(
        {
            "CARGO_BUILD_JOBS": "2",
            "CARGO_TERM_COLOR": "never",
        }
    )
    executor.run(
        identity.command(
            [
                str(cargo_binary),
                "generate-lockfile",
                "--offline",
                "--manifest-path",
                str(rust_consumer / "Cargo.toml"),
            ]
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=rust_consumer,
        env=cargo_environment,
    )
    metadata = executor.run(
        identity.command(
            [
                str(cargo_binary),
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--offline",
                "--manifest-path",
                str(rust_consumer / "Cargo.toml"),
            ]
        ),
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=rust_consumer,
        env=cargo_environment,
    ).stdout
    validate_cargo_artifact_origins(metadata, rust_artifacts, repository_root)
    executor.run(
        identity.command(
            [
                "flock",
                HEAVY_BUILD_LOCK,
                str(cargo_binary),
                "build",
                "--locked",
                "--offline",
                "--jobs",
                "2",
                "--target-dir",
                str(rust_target),
                "--manifest-path",
                str(rust_consumer / "Cargo.toml"),
            ]
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=rust_consumer,
        env=cargo_environment,
    )

    executor.run(
        identity.command(
            [
                "npm",
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--package-lock=false",
                str(artifacts.npm_tarball),
            ]
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=typescript_root,
    )

    python_commands: list[tuple[str, ...]] = []
    python_roots: list[Path] = []
    for root, archive in (
        (wheel_root, artifacts.python_wheel),
        (sdist_root, artifacts.python_sdist),
    ):
        site = root / "site"
        install_command = [
            sys.executable,
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-deps",
            "--no-index",
            "--target",
            str(site),
        ]
        if archive == artifacts.python_sdist:
            install_command.append("--no-build-isolation")
        install_command.append(str(archive))
        executor.run(
            identity.command(install_command),
            timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
            cwd=root,
        )
        python_commands.append((sys.executable, str(root / "quickstart.py")))
        python_roots.append(site)

    rust_binary = rust_target / "debug" / "xenoteer-public-quickstart"
    if not rust_binary.is_file():
        raise GateError("staged Rust quick-start build omitted its binary")
    return InstalledQuickstarts(
        (str(rust_binary),),
        rust_artifacts,
        ("node", str(typescript_root / "quickstart.mjs")),
        typescript_root / "node_modules" / "@xenoteer" / "sdk",
        python_commands[0],
        python_roots[0],
        python_commands[1],
        python_roots[1],
    )


def _docker_inspect(
    executor: CommandExecutor,
    arguments: Sequence[str],
) -> str:
    return executor.run(
        ["docker", *arguments],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
    ).stdout.strip()


def resolve_exact_image(
    executor: CommandExecutor,
    image_reference: str,
) -> tuple[str, str]:
    """Resolve one image once and read its recorded source identity."""

    image_id = validate_image_id(
        _docker_inspect(
            executor,
            ["image", "inspect", image_reference, "--format", "{{.Id}}"],
        )
    )
    source_tree_sha256 = _docker_inspect(
        executor,
        [
            "image",
            "inspect",
            image_id,
            "--format",
            '{{index .Config.Labels "com.aeor.xenoteer.source-tree.sha256"}}',
        ],
    )
    if re.fullmatch(r"[0-9a-f]{64}", source_tree_sha256) is None:
        raise GateError("release-candidate image omitted its exact source-tree hash label")
    return image_id, source_tree_sha256


def _root_owned_token_supported(executor: CommandExecutor) -> bool:
    if os.geteuid() == 0:
        return True
    security = executor.run(
        ["docker", "info", "--format", "{{json .SecurityOptions}}"],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
    ).stdout
    return "name=rootless" in security


def wait_until_ready(
    executor: CommandExecutor,
    container_name: str,
    api_base: str,
) -> None:
    """Bound readiness while detecting an early container exit."""

    deadline = time.monotonic() + READINESS_TIMEOUT_SECONDS
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while time.monotonic() < deadline:
        try:
            with opener.open(f"{api_base}/readyz", timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
        running = _docker_inspect(
            executor,
            ["inspect", container_name, "--format", "{{.State.Running}}"],
        )
        if running != "true":
            raise GateError("release-candidate container stopped before readiness")
        time.sleep(1)
    raise GateError("release-candidate container did not become ready within 90 seconds")


def run_one_quickstart(
    executor: CommandExecutor,
    *,
    name: str,
    command: Sequence[str],
    installed_root: Path,
    api_base: str,
    token: str,
    expect_auth_failure: bool,
    forbidden_tokens: Sequence[str],
) -> None:
    """Run one installed consumer with bounded typed-auth expectations."""

    environment = dict(os.environ)
    for unsafe in ("PYTHONPATH", "PYTHONHOME", "NODE_PATH"):
        environment.pop(unsafe, None)
    environment.update(
        {
            "XENOTEER_API_BASE": api_base,
            "XENOTEER_TOKEN": token,
            "XENOTEER_EXPECTED_INSTALL_ROOT": str(installed_root),
            "XENOTEER_EXPECT_AUTH_FAILURE": "1" if expect_auth_failure else "0",
            "XENOTEER_QUICKSTART_LANGUAGE": name,
            "PYTHONNOUSERSITE": "1",
            "RUST_BACKTRACE": "0",
        }
    )
    if name.startswith("python-"):
        environment["PYTHONPATH"] = str(installed_root)
    result = executor.run(
        ["nice", "-n", "15", "ionice", "-c", "3", *command],
        timeout=QUICKSTART_COMMAND_TIMEOUT_SECONDS,
        check=False,
        env=environment,
    )
    combined = result.stdout + result.stderr
    if any(secret in combined for secret in forbidden_tokens):
        raise GateError(f"{name} quick-start exposed a bearer canary")
    mode = "auth-failure" if expect_auth_failure else "success"
    marker = f"quickstart-ok language={name} mode={mode}"
    if result.returncode != 0 or marker not in result.stdout:
        raise GateError(
            f"{name} {mode} quick-start failed: "
            f"{safe_diagnostic(combined) or 'no safe diagnostic'}"
        )


def assert_container_logs_safe(
    executor: CommandExecutor,
    container_name: str,
    forbidden_tokens: Sequence[str],
) -> None:
    """Require readable logs with neither valid nor invalid bearer canary."""

    logs = executor.run(
        ["docker", "logs", container_name],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        check=False,
    )
    if logs.returncode != 0:
        raise GateError("could not inspect release-candidate container logs")
    if any(secret in logs.stdout + logs.stderr for secret in forbidden_tokens):
        raise GateError("release-candidate container logs exposed a bearer canary")


def run_live_gate(
    repository_root: Path,
    workspace: Path,
    artifacts: PublicArtifacts,
    installed: InstalledQuickstarts,
    image_id: str,
    executor: CommandExecutor,
) -> None:
    """Exercise every archive variant against one exact running image."""

    token = "PHASE6_PUBLIC_QUICKSTART_TOKEN_" + secrets.token_hex(24)
    wrong_token = "PHASE6_PUBLIC_QUICKSTART_WRONG_" + secrets.token_hex(24)
    token_file = workspace / "api-token"
    token_file.write_text(token, encoding="ascii")
    token_file.chmod(0o400)
    if not _root_owned_token_supported(executor):
        raise GateError(
            "run as root or use rootless Docker so the token maps to container UID 0"
        )

    container_name = f"xenoteer-phase6-quickstart-{os.getpid()}-{secrets.token_hex(4)}"
    with ContainerGuard(executor, container_name) as guard:
        # Arm cleanup before Docker can create the named container and then lose
        # its response or exceed the subprocess deadline.
        guard.mark_created()
        executor.run(
            [
                "docker",
                "run",
                "--detach",
                "--name",
                container_name,
                "--cpus",
                "2",
                "--memory",
                "6g",
                "--pids-limit",
                "512",
                "--shm-size",
                "4g",
                "--log-driver",
                "json-file",
                "--log-opt",
                "max-size=2m",
                "--log-opt",
                "max-file=1",
                "--publish",
                "127.0.0.1::8080",
                "--volume",
                f"{token_file}:/run/secrets/xenoteer_api_token:ro",
                image_id,
            ],
            timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        )
        running_image = validate_image_id(
            _docker_inspect(
                executor,
                ["inspect", container_name, "--format", "{{.Image}}"],
            )
        )
        if running_image != image_id:
            raise GateError("Docker container did not retain the resolved immutable image ID")
        port = parse_loopback_port(
            _docker_inspect(executor, ["port", container_name, "8080/tcp"])
        )
        api_base = f"http://127.0.0.1:{port}"
        wait_until_ready(executor, container_name, api_base)

        for name, command, installed_root in installed.variants():
            run_one_quickstart(
                executor,
                name=name,
                command=command,
                installed_root=installed_root,
                api_base=api_base,
                token=wrong_token,
                expect_auth_failure=True,
                forbidden_tokens=(token, wrong_token),
            )
            run_one_quickstart(
                executor,
                name=name,
                command=command,
                installed_root=installed_root,
                api_base=api_base,
                token=token,
                expect_auth_failure=False,
                forbidden_tokens=(token, wrong_token),
            )

        assert_container_logs_safe(
            executor,
            container_name,
            (token, wrong_token),
        )

        stop = executor.run(
            ["docker", "stop", "--time", "8", container_name],
            timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            check=False,
        )
        if stop.returncode != 0:
            raise GateError("release-candidate container did not stop within the cleanup bound")
        exit_code = _docker_inspect(
            executor,
            ["inspect", container_name, "--format", "{{.State.ExitCode}}"],
        )
        if exit_code != "0":
            raise GateError(
                f"release-candidate container returned nonzero after quick-starts: {exit_code}"
            )
        assert_container_logs_safe(
            executor,
            container_name,
            (token, wrong_token),
        )

    final_source_tree_sha256 = current_source_tree_hash(repository_root, executor)
    if final_source_tree_sha256 != artifacts.source_tree_sha256:
        raise GateError("source tree changed while the public quick-start gate was running")


def qualify(image_reference: str) -> dict[str, str]:
    """Run the complete gate and return exact identities only after success."""

    reject_daemon_overrides(os.environ)
    repository_root = Path(__file__).resolve().parents[2]
    executor = CommandExecutor()
    identity = BuildIdentity.current()
    image_id, image_source_tree_sha256 = resolve_exact_image(executor, image_reference)
    current_hash = current_source_tree_hash(repository_root, executor)
    if current_hash != image_source_tree_sha256:
        raise GateError(
            "release-candidate image source identity differs from the current package tree"
        )

    with tempfile.TemporaryDirectory(
        prefix="xenoteer-phase6-public-quickstarts-"
    ) as temporary:
        workspace = Path(temporary)
        chown_tree(workspace, identity)
        artifacts = stage_public_artifacts(
            repository_root,
            workspace,
            current_hash,
            executor,
            identity,
        )
        after_staging_hash = current_source_tree_hash(repository_root, executor)
        if after_staging_hash != image_source_tree_sha256:
            raise GateError(
                "source tree changed after the release-candidate image or during packaging"
            )
        installed = prepare_installed_quickstarts(
            repository_root,
            workspace,
            artifacts,
            executor,
            identity,
        )
        before_live_hash = current_source_tree_hash(repository_root, executor)
        if before_live_hash != image_source_tree_sha256:
            raise GateError(
                "source tree changed while installed quick-start consumers were prepared"
            )
        run_live_gate(
            repository_root,
            workspace,
            artifacts,
            installed,
            image_id,
            executor,
        )
        return {
            "image": image_id,
            "source_tree": image_source_tree_sha256,
            **{
                name: f"sha256:{digest}"
                for name, digest in artifacts.digests().items()
            },
        }


def usage_error(message: str) -> NoReturn:
    print(message, file=sys.stderr)
    print("usage: test-public-quickstarts.py IMAGE", file=sys.stderr)
    raise SystemExit(64)


def _raise_interrupted(
    signal_number: int,
    frame: object,
) -> NoReturn:
    del signal_number, frame
    raise KeyboardInterrupt


def main(arguments: Sequence[str] | None = None) -> int:
    """CLI entry point."""

    arguments = list(sys.argv[1:] if arguments is None else arguments)
    if len(arguments) != 1 or arguments[0].startswith("-"):
        usage_error("exactly one local release-candidate image reference is required")
    previous_sigterm = signal.signal(signal.SIGTERM, _raise_interrupted)
    try:
        identities = qualify(arguments[0])
    except GateError as error:
        print(f"public quick-start qualification failed: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("public quick-start qualification interrupted after cleanup", file=sys.stderr)
        return 130
    finally:
        signal.signal(signal.SIGTERM, previous_sigterm)
    summary = " ".join(f"{name}={identities[name]}" for name in sorted(identities))
    print(f"public quick-start qualification passed: {summary}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
