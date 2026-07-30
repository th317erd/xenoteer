#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Install staged public SDK artifacts and qualify them against one exact image."""

from __future__ import annotations

import dataclasses
import importlib.util
import json
import os
import pwd
import re
import secrets
import selectors
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

try:
    from scripts.sdk import qualification_identity as _qualification_identity
except ModuleNotFoundError as error:
    if error.name not in {
        "scripts",
        "scripts.sdk",
        "scripts.sdk.qualification_identity",
    }:
        raise
    module_name = "qualification_identity"
    existing_identity_module = sys.modules.get(module_name)
    if existing_identity_module is not None:
        _qualification_identity = existing_identity_module
    else:
        identity_path = Path(__file__).with_name("qualification_identity.py")
        identity_spec = importlib.util.spec_from_file_location(
            module_name,
            identity_path,
        )
        if identity_spec is None or identity_spec.loader is None:
            raise RuntimeError("could not load release identity module")
        _qualification_identity = importlib.util.module_from_spec(identity_spec)
        sys.modules[module_name] = _qualification_identity
        identity_spec.loader.exec_module(_qualification_identity)


DEFAULT_COMMAND_TIMEOUT_SECONDS = (
    _qualification_identity.DEFAULT_COMMAND_TIMEOUT_SECONDS
)
PACKAGE_COMMAND_TIMEOUT_SECONDS = 120
QUICKSTART_COMMAND_TIMEOUT_SECONDS = 120
READINESS_TIMEOUT_SECONDS = 90
HEAVY_BUILD_LOCK = "/tmp/codex/xenoteer-heavy-build.lock"
ENV_BINARY = Path("/usr/bin/env")
NICE_BINARY = Path("/usr/bin/nice")
IONICE_BINARY = Path("/usr/bin/ionice")
SUDO_BINARY = Path("/usr/bin/sudo")
TRUSTED_SYSTEM_PATH = "/usr/sbin:/usr/bin:/sbin:/bin"
PACKAGE_BUILD_PATH_ENVIRONMENT = "XENOTEER_PACKAGE_BUILD_PATH"
SUPPORTED_NODE_MAJORS = frozenset({22, 24})
NODE_VERSION_DECIMAL = r"(?:0|[1-9][0-9]{0,9})"
STABLE_NODE_VERSION = re.compile(
    rf"v(?P<major>{NODE_VERSION_DECIMAL})"
    rf"\.(?P<minor>{NODE_VERSION_DECIMAL})"
    rf"\.(?P<patch>{NODE_VERSION_DECIMAL})\Z"
)
NODE_PROBE_TIMEOUT_SECONDS = 5.0
NODE_PROBE_OUTPUT_LIMIT_BYTES = 4 * 1024
NODE_PROBE_CLEANUP_GRACE_SECONDS = 0.25
NPM_WRAPPER_MAX_BYTES = 64 * 1024
NPM_ENV_NODE_SHEBANG = b"#!/usr/bin/env node\n"
LOOPBACK_PORT = re.compile(r"127\.0\.0\.1:([0-9]{1,5})\Z")
DAEMON_OVERRIDE_ENVIRONMENTS = _qualification_identity.DAEMON_OVERRIDE_ENVIRONMENTS
REQUIRED_BEHAVIORS = (
    "status-capabilities",
    "scoped-lease-fixture-launch",
    "exact-window-element",
    "semantic-invoke",
    "smooth-physical-click-postcondition",
    "unicode-text-strategy",
    "screenshot-on-failure",
    "reconnect-known-command",
    "stale-reference-restart",
    "view-only-browser-ticket",
)
FIRST_PARTY_MANIFEST_PATH = "/usr/share/doc/xenoteer/first-party-files.tsv"
RUNTIME_EVIDENCE_PATHS = (
    "/etc/s6-overlay/s6-rc.d",
    "/etc/xenoteer",
    "/usr/local/libexec/xenoteer",
    "/usr/share/doc/xenoteer",
    "/usr/share/novnc/mandatory.json",
    "/usr/share/xenoteer",
    "/etc/at-spi2/accessibility.conf",
    "/etc/dbus-1/session-local.conf",
    "/usr/local/bin/xenoteerd",
    "/usr/local/bin/xenoteer-processd",
)
CRITICAL_RUNTIME_PATHS = (
    "/etc/s6-overlay/s6-rc.d",
    "/etc/xenoteer",
    "/usr/local/libexec/xenoteer",
    "/usr/share/novnc/mandatory.json",
    "/usr/share/xenoteer",
    "/etc/at-spi2/accessibility.conf",
    "/etc/dbus-1/session-local.conf",
    "/usr/local/bin/xenoteerd",
    "/usr/local/bin/xenoteer-processd",
)
FIXTURE_IMAGE_PATH = "/usr/share/xenoteer/fixtures/desktop-apps"
FIXTURE_REPOSITORY_PATH = "container/rootfs/usr/share/xenoteer/fixtures/desktop-apps"
FIXTURE_ARTIFACT_LOCK_IMAGE_PATH = (
    "/usr/share/doc/xenoteer/desktop-app-artifacts.lock"
)
FIXTURE_ARTIFACT_LOCK_REPOSITORY_PATH = (
    "container/fixtures/desktop-apps/artifacts.lock"
)
FIXTURE_DEBIAN_SNAPSHOT = _qualification_identity.FIXTURE_DEBIAN_SNAPSHOT
FIXTURE_ONLY_LABELS = _qualification_identity.FIXTURE_ONLY_LABELS
GateError = _qualification_identity.GateError
UntrackedSource = _qualification_identity.UntrackedSource
SourceIdentity = _qualification_identity.SourceIdentity
FixtureArtifactLock = _qualification_identity.FixtureArtifactLock
ExactFixtureImage = _qualification_identity.ExactFixtureImage
reject_daemon_overrides = _qualification_identity.reject_daemon_overrides
validate_image_id = _qualification_identity.validate_image_id
validate_exact_image_ids = _qualification_identity.validate_exact_image_ids
source_snapshot_digest = _qualification_identity.source_snapshot_digest
current_source_identity = _qualification_identity.current_source_identity
current_source_tree_hash = _qualification_identity.current_source_tree_hash
file_sha256 = _qualification_identity.file_sha256
validate_dependency_lock_digest = (
    _qualification_identity.validate_dependency_lock_digest
)
current_dependency_lock_digest = (
    _qualification_identity.current_dependency_lock_digest
)
parse_fixture_artifact_lock = _qualification_identity.parse_fixture_artifact_lock
read_fixture_artifact_lock = _qualification_identity.read_fixture_artifact_lock
validate_fixture_image_metadata = (
    _qualification_identity.validate_fixture_image_metadata
)
validate_release_image_metadata = (
    _qualification_identity.validate_release_image_metadata
)


@dataclasses.dataclass(frozen=True)
class CommandResult:
    """Captured bounded subprocess result."""

    returncode: int
    stdout: str
    stderr: str


@dataclasses.dataclass(frozen=True)
class PackageToolchain:
    """One immutable, at-use-validated Node/npm selection."""

    node: Path
    npm: Path
    git: Path
    path: tuple[Path, ...]
    version: tuple[int, int, int]

    def source_environment(self) -> dict[str, str]:
        return {
            "PATH": TRUSTED_SYSTEM_PATH,
            PACKAGE_BUILD_PATH_ENVIRONMENT: os.pathsep.join(
                str(directory) for directory in self.path
            ),
        }


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

    def _trusted_path(
        self,
        source_environment: Mapping[str, str] | None = None,
    ) -> tuple[Path, ...]:
        environment = os.environ if source_environment is None else source_environment
        raw_path = environment.get(PACKAGE_BUILD_PATH_ENVIRONMENT)
        if raw_path is None:
            raw_path = environment.get("PATH")
        if not isinstance(raw_path, str) or not raw_path:
            raise GateError("package build PATH is missing")
        directories: list[Path] = []
        observed: set[Path] = set()
        for raw_directory in raw_path.split(os.pathsep):
            directory = Path(raw_directory)
            if not raw_directory or not directory.is_absolute():
                raise GateError("package build PATH contains an empty or relative entry")
            try:
                resolved = directory.resolve(strict=True)
            except OSError:
                continue
            if not self._is_trusted_directory(resolved, allowed_owners={0, self.uid}):
                continue
            if resolved not in observed:
                directories.append(resolved)
                observed.add(resolved)
        if not directories:
            raise GateError("package build PATH contains no trusted directories")
        return tuple(directories)

    def _target_has_permission(
        self,
        metadata: os.stat_result,
        owner_mask: int,
        group_mask: int,
        other_mask: int,
    ) -> bool:
        """Evaluate one mode permission as the post-sudo build identity."""

        if metadata.st_uid == self.uid:
            return bool(metadata.st_mode & owner_mask)
        if metadata.st_gid == self.gid:
            return bool(metadata.st_mode & group_mask)
        return bool(metadata.st_mode & other_mask)

    def _target_can_traverse(self, directory: Path) -> bool:
        for component in (directory, *directory.parents):
            try:
                metadata = component.stat()
            except OSError:
                return False
            if not stat.S_ISDIR(metadata.st_mode) or not self._target_has_permission(
                metadata,
                stat.S_IXUSR,
                stat.S_IXGRP,
                stat.S_IXOTH,
            ):
                return False
        return True

    def _target_writers_are_trusted(self, metadata: os.stat_result) -> bool:
        """Accept writes only by the target owner and its primary group."""

        if metadata.st_mode & stat.S_IWOTH:
            return False
        if metadata.st_mode & stat.S_IWGRP:
            return metadata.st_uid == self.uid and metadata.st_gid == self.gid
        return True

    def _is_trusted_directory(
        self,
        directory: Path,
        *,
        allowed_owners: set[int],
    ) -> bool:
        try:
            metadata = directory.stat()
        except OSError:
            return False
        return (
            stat.S_ISDIR(metadata.st_mode)
            and metadata.st_uid in allowed_owners
            and self._target_writers_are_trusted(metadata)
            and self._target_can_traverse(directory)
        )

    def _is_trusted_executable(
        self,
        candidate: Path,
        *,
        allowed_owners: set[int],
    ) -> bool:
        try:
            candidate_parent = candidate.parent.resolve(strict=True)
            target = candidate.resolve(strict=True)
            target_parent = target.parent.resolve(strict=True)
            metadata = target.stat()
        except OSError:
            return False
        if any(
            not self._is_trusted_directory(
                directory,
                allowed_owners=allowed_owners,
            )
            for directory in {candidate_parent, target_parent}
        ):
            return False
        return (
            stat.S_ISREG(metadata.st_mode)
            and metadata.st_uid in allowed_owners
            and self._target_writers_are_trusted(metadata)
            and self._target_has_permission(
                metadata,
                stat.S_IXUSR,
                stat.S_IXGRP,
                stat.S_IXOTH,
            )
        )

    def _trusted_home(self) -> Path:
        if not self.home.is_absolute():
            raise GateError("package build HOME must be absolute")
        try:
            resolved = self.home.resolve(strict=True)
            metadata = resolved.stat()
        except OSError as error:
            raise GateError("package build HOME is unavailable") from error
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid not in {0, self.uid}
            or not self._target_writers_are_trusted(metadata)
            or not self._target_can_traverse(resolved)
        ):
            raise GateError("package build HOME is untrusted or inaccessible")
        return self.home

    def _trusted_installed_root(self, installed_root: Path) -> Path:
        if not installed_root.is_absolute():
            raise GateError(
                "installed root is unavailable, untrusted, or inaccessible"
            )
        try:
            resolved = installed_root.resolve(strict=True)
        except OSError as error:
            raise GateError(
                "installed root is unavailable, untrusted, or inaccessible"
            ) from error
        if not self._is_trusted_directory(
            resolved,
            allowed_owners={0, self.uid},
        ):
            raise GateError(
                "installed root is unavailable, untrusted, or inaccessible"
            )
        return resolved

    def _fixed_executable(self, executable: Path) -> Path:
        if not executable.is_absolute() or not self._is_trusted_executable(
            executable,
            allowed_owners={0},
        ):
            raise GateError(
                f"required package build wrapper is unavailable: {executable.name}"
            )
        return executable

    def resolve_executable(
        self,
        executable: str,
        *,
        source_environment: Mapping[str, str] | None = None,
    ) -> Path:
        """Resolve one executable before dropping privileges.

        The returned path preserves a reviewed symlink name (notably npm and
        rustup proxy shims), while trust is checked against its final target.
        """

        if not executable or "\0" in executable:
            raise GateError("package build executable name is invalid")
        trusted_path = self._trusted_path(source_environment)
        requested = Path(executable)
        if requested.is_absolute():
            candidates = (requested,)
        elif requested.parent == Path(".") and requested.name == executable:
            candidates = tuple(directory / executable for directory in trusted_path)
        else:
            raise GateError("package build executable must be absolute or a bare name")
        for candidate in candidates:
            if self._is_trusted_executable(
                candidate,
                allowed_owners={0, self.uid},
            ):
                return candidate
        raise GateError(f"required package build executable is unavailable: {requested.name}")

    def resolve_system_executable(
        self,
        executable: str,
        *,
        source_environment: Mapping[str, str] | None = None,
    ) -> Path:
        """Resolve one root-owned executable only from the fixed system path."""

        del source_environment
        requested = Path(executable)
        if (
            not executable
            or "\0" in executable
            or requested.is_absolute()
            or requested.parent != Path(".")
            or requested.name != executable
        ):
            raise GateError("system executable must be one bare name")
        for raw_directory in TRUSTED_SYSTEM_PATH.split(os.pathsep):
            directory = Path(raw_directory)
            if not raw_directory or not directory.is_absolute():
                raise GateError("trusted system PATH is malformed")
            try:
                resolved_directory = directory.resolve(strict=True)
            except OSError:
                continue
            if not self._is_trusted_directory(
                resolved_directory,
                allowed_owners={0},
            ):
                continue
            candidate = resolved_directory / executable
            if self._is_trusted_executable(
                candidate,
                allowed_owners={0},
            ):
                return candidate
        raise GateError(f"required system executable is unavailable: {requested.name}")

    def command(
        self,
        command: Sequence[str],
        *,
        source_environment: Mapping[str, str] | None = None,
    ) -> list[str]:
        if not command:
            raise GateError("package build command is empty")
        home = self._trusted_home()
        trusted_path = self._trusted_path(source_environment)
        executable = self.resolve_executable(
            command[0],
            source_environment=source_environment,
        )
        env_binary = self._fixed_executable(ENV_BINARY)
        nice_binary = self._fixed_executable(NICE_BINARY)
        ionice_binary = self._fixed_executable(IONICE_BINARY)
        clean_environment = [
            str(env_binary),
            "-i",
            f"HOME={home}",
            "PATH=" + os.pathsep.join(str(directory) for directory in trusted_path),
        ]
        low_priority = [
            *clean_environment,
            str(nice_binary),
            "-n",
            "15",
            str(ionice_binary),
            "-c",
            "3",
            str(executable),
            *command[1:],
        ]
        if not self.use_sudo:
            return low_priority
        sudo_binary = self._fixed_executable(SUDO_BINARY)
        return [str(sudo_binary), "-H", "-u", f"#{self.uid}", "--", *low_priority]

    def runtime_command(
        self,
        command: Sequence[str],
        *,
        source_environment: Mapping[str, str] | None = None,
    ) -> list[str]:
        """Validate and lower the priority of one installed quick-start.

        Unlike package-build commands, runtime bearer credentials cannot pass
        through ``env KEY=value`` argv. ``CommandExecutor.run_as_identity``
        supplies the exact environment and performs the credential transition.
        """

        if not command:
            raise GateError("installed quick-start command is empty")
        requested = Path(command[0])
        if not requested.is_absolute():
            raise GateError("installed quick-start executable must be absolute")
        executable = self.resolve_executable(
            command[0],
            source_environment=source_environment,
        )
        nice_binary = self._fixed_executable(NICE_BINARY)
        ionice_binary = self._fixed_executable(IONICE_BINARY)
        return [
            str(nice_binary),
            "-n",
            "15",
            str(ionice_binary),
            "-c",
            "3",
            str(executable),
            *command[1:],
        ]


class CommandExecutor:
    """Timeout-enforcing subprocess boundary."""

    @staticmethod
    def _root_environment(
        source_environment: Mapping[str, str] | None,
    ) -> dict[str, str]:
        del source_environment
        try:
            account = pwd.getpwuid(os.geteuid())
        except KeyError as error:
            raise GateError("root subprocess identity has no local account") from error
        if not account.pw_dir.startswith("/"):
            raise GateError("root subprocess identity has an invalid home")
        return {
            "HOME": account.pw_dir,
            "LANG": "C",
            "LC_ALL": "C",
            "LOGNAME": account.pw_name,
            "PATH": TRUSTED_SYSTEM_PATH,
            "USER": account.pw_name,
        }

    @staticmethod
    def _credential_options(
        identity: BuildIdentity | None,
    ) -> dict[str, object]:
        if identity is None:
            return {}
        if identity.uid <= 0:
            raise GateError("quick-start target identity cannot be root")
        if identity.gid <= 0:
            raise GateError("quick-start target group is invalid")
        if identity.use_sudo:
            if os.geteuid() != 0:
                raise GateError(
                    "sudo quick-start boundary requires a root outer process"
                )
            return {
                "user": identity.uid,
                "group": identity.gid,
                "extra_groups": (),
            }
        if os.geteuid() != identity.uid or os.getegid() != identity.gid:
            raise GateError(
                "rootless quick-start identity differs from current process identity"
            )
        return {}

    def _child_environment(
        self,
        source_environment: Mapping[str, str] | None,
        identity: BuildIdentity | None,
    ) -> dict[str, str]:
        if identity is None:
            return self._root_environment(source_environment)
        return {} if source_environment is None else dict(source_environment)

    def _run(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        check: bool,
        cwd: Path | None,
        env: Mapping[str, str] | None,
        identity: BuildIdentity | None,
    ) -> CommandResult:
        if timeout <= 0 or timeout > PACKAGE_COMMAND_TIMEOUT_SECONDS:
            raise GateError(f"invalid subprocess timeout: {timeout}")
        if not command:
            raise GateError("subprocess command is empty")
        credential_options = self._credential_options(identity)
        try:
            completed = subprocess.run(
                list(command),
                cwd=cwd,
                env=self._child_environment(env, identity),
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
                **credential_options,
            )
        except FileNotFoundError as error:
            raise GateError(f"required command is unavailable: {command[0]}") from error
        except PermissionError as error:
            raise GateError(
                f"could not launch command: {safe_command(command)}"
            ) from error
        except subprocess.TimeoutExpired as error:
            raise GateError(
                f"command exceeded its {timeout}s bound: {safe_command(command)}"
            ) from error
        except UnicodeError as error:
            raise GateError(
                f"command returned invalid UTF-8: {safe_command(command)}"
            ) from error
        except ValueError as error:
            raise GateError(
                f"could not launch command: {safe_command(command)}"
            ) from error
        except OSError as error:
            raise GateError(
                f"could not launch command: {safe_command(command)}"
            ) from error
        if not isinstance(completed.stdout, str) or not isinstance(
            completed.stderr,
            str,
        ):
            raise GateError(
                f"command returned invalid text output: {safe_command(command)}"
            )
        result = CommandResult(completed.returncode, completed.stdout, completed.stderr)
        if check and result.returncode != 0:
            detail = safe_diagnostic(result.stderr or result.stdout)
            raise GateError(
                f"command failed ({safe_command(command)}): {detail or 'no safe diagnostic'}"
            )
        return result

    def run(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        check: bool = True,
        cwd: Path | None = None,
        env: Mapping[str, str] | None = None,
    ) -> CommandResult:
        return self._run(
            command,
            timeout=timeout,
            check=check,
            cwd=cwd,
            env=env,
            identity=None,
        )

    def run_as_identity(
        self,
        command: Sequence[str],
        *,
        identity: BuildIdentity,
        timeout: int,
        env: Mapping[str, str],
        check: bool = True,
        cwd: Path | None = None,
    ) -> CommandResult:
        """Run only an installed quick-start as its non-root build identity."""

        return self._run(
            command,
            timeout=timeout,
            check=check,
            cwd=cwd,
            env=env,
            identity=identity,
        )

    @staticmethod
    def _terminate_probe_group(process: subprocess.Popen[bytes]) -> None:
        """Terminate and reap one probe session, including pipe-holding children."""

        termination_error: GateError | None = None
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except OSError as error:
            termination_error = GateError(
                "node version probe group could not receive TERM"
            )
            termination_error.__cause__ = error
        time.sleep(NODE_PROBE_CLEANUP_GRACE_SECONDS)
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            if termination_error is None:
                termination_error = GateError(
                    "node version probe group could not receive KILL"
                )
                termination_error.__cause__ = error
            try:
                process.kill()
            except OSError:
                pass
        try:
            process.wait(timeout=NODE_PROBE_CLEANUP_GRACE_SECONDS)
        except subprocess.TimeoutExpired as error:
            raise GateError("node version probe could not be reaped") from error
        except OSError as error:
            raise GateError("node version probe leader could not be reaped") from error
        if termination_error is not None:
            raise termination_error

    @staticmethod
    def _drain_probe_pipes(
        selector: selectors.BaseSelector,
    ) -> None:
        """Drain only a small, post-kill window so inherited pipes cannot hang."""

        deadline = time.monotonic() + NODE_PROBE_CLEANUP_GRACE_SECONDS
        while selector.get_map() and time.monotonic() < deadline:
            events = selector.select(max(0.0, deadline - time.monotonic()))
            if not events:
                break
            for key, _ in events:
                try:
                    chunk = os.read(key.fd, 4_096)
                except OSError:
                    chunk = b""
                if not chunk:
                    try:
                        selector.unregister(key.fileobj)
                    except KeyError:
                        pass

    def run_probe(
        self,
        command: Sequence[str],
        *,
        timeout: float,
        output_limit: int,
        cwd: Path | None = None,
    ) -> CommandResult:
        """Run one hostile-output probe with a process-group and byte deadline."""

        if timeout <= 0 or timeout > DEFAULT_COMMAND_TIMEOUT_SECONDS:
            raise GateError(f"invalid node probe timeout: {timeout}")
        if output_limit <= 0 or output_limit > NODE_PROBE_OUTPUT_LIMIT_BYTES:
            raise GateError(f"invalid node probe output limit: {output_limit}")
        if not command:
            raise GateError("node version probe command is empty")

        process: subprocess.Popen[bytes] | None = None
        selector = selectors.DefaultSelector()
        streams: tuple[object, ...] = ()
        terminated = False
        try:
            process = subprocess.Popen(
                list(command),
                cwd=cwd,
                env=self._root_environment(None),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
            if process.stdout is None or process.stderr is None:
                raise GateError("node version probe did not expose bounded pipes")
            streams = (process.stdout, process.stderr)
            selector.register(process.stdout, selectors.EVENT_READ, "stdout")
            selector.register(process.stderr, selectors.EVENT_READ, "stderr")
            captured = {"stdout": bytearray(), "stderr": bytearray()}
            total_output = 0
            deadline = time.monotonic() + timeout
            failure: GateError | None = None

            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    failure = GateError(
                        f"node version probe exceeded its {timeout:g}s bound"
                    )
                    break
                events = selector.select(min(0.05, remaining))
                for key, _ in events:
                    try:
                        chunk = os.read(key.fd, 4_096)
                    except OSError as error:
                        failure = GateError("node version probe output could not be read")
                        failure.__cause__ = error
                        break
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    total_output += len(chunk)
                    if total_output > output_limit:
                        failure = GateError(
                            "node version probe exceeded its output limit"
                        )
                        break
                    captured[str(key.data)].extend(chunk)
                if failure is not None:
                    break

            # Signal the session before wait(2) reaps its leader. A zombie leader
            # keeps the PID/PGID reserved while any descendant is terminated, so
            # no post-reap PID-reuse window can target an unrelated process group.
            self._terminate_probe_group(process)
            terminated = True
            self._drain_probe_pipes(selector)
            if failure is not None:
                raise failure

            try:
                stdout = bytes(captured["stdout"]).decode("utf-8")
                stderr = bytes(captured["stderr"]).decode("utf-8")
            except UnicodeDecodeError as error:
                raise GateError("node version probe returned invalid UTF-8") from error
            result = CommandResult(process.returncode, stdout, stderr)
            if result.returncode != 0:
                detail = safe_diagnostic(result.stderr or result.stdout)
                raise GateError(
                    "node version probe failed: "
                    f"{detail or 'no safe diagnostic'}"
                )
            return result
        except (FileNotFoundError, PermissionError) as error:
            raise GateError(
                f"could not launch node version probe: {safe_command(command)}"
            ) from error
        except OSError as error:
            raise GateError(
                "node version probe failed during bounded execution"
            ) from error
        finally:
            if process is not None and not terminated:
                self._terminate_probe_group(process)
                self._drain_probe_pipes(selector)
            selector.close()
            for stream in streams:
                stream.close()  # type: ignore[union-attr]

    def run_bytes(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> bytes:
        return self._run_bytes(
            command,
            timeout=timeout,
            cwd=cwd,
            env=None,
            identity=None,
        )

    def run_bytes_as_identity(
        self,
        command: Sequence[str],
        *,
        identity: BuildIdentity,
        timeout: int,
        env: Mapping[str, str],
        cwd: Path | None = None,
    ) -> bytes:
        """Run a byte-exact command as the validated non-root build identity."""

        return self._run_bytes(
            command,
            timeout=timeout,
            cwd=cwd,
            env=env,
            identity=identity,
        )

    def _run_bytes(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None,
        env: Mapping[str, str] | None,
        identity: BuildIdentity | None,
    ) -> bytes:
        if timeout <= 0 or timeout > DEFAULT_COMMAND_TIMEOUT_SECONDS:
            raise GateError(f"invalid binary subprocess timeout: {timeout}")
        if not command:
            raise GateError("binary subprocess command is empty")
        credential_options = self._credential_options(identity)
        try:
            completed = subprocess.run(
                list(command),
                cwd=cwd,
                env=self._child_environment(env, identity),
                capture_output=True,
                text=False,
                timeout=timeout,
                check=False,
                **credential_options,
            )
        except FileNotFoundError as error:
            raise GateError(f"required command is unavailable: {command[0]}") from error
        except PermissionError as error:
            raise GateError(
                f"could not launch command: {safe_command(command)}"
            ) from error
        except ValueError as error:
            raise GateError(
                f"could not launch command: {safe_command(command)}"
            ) from error
        except subprocess.TimeoutExpired as error:
            raise GateError(
                f"command exceeded its {timeout}s bound: {safe_command(command)}"
            ) from error
        except OSError as error:
            raise GateError(
                f"could not launch command: {safe_command(command)}"
            ) from error
        if completed.returncode != 0:
            raise GateError(f"command failed: {safe_command(command)}")
        if not isinstance(completed.stdout, bytes):
            raise GateError(
                f"command returned invalid binary output: {safe_command(command)}"
            )
        return completed.stdout


@dataclasses.dataclass(frozen=True)
class SourceIdentityExecutor:
    """Run shared source-identity Git reads as the validated invoking user."""

    identity: BuildIdentity
    executor: CommandExecutor
    git: Path
    _environment: tuple[tuple[str, str], ...] = dataclasses.field(
        init=False,
        repr=False,
    )

    def __post_init__(self) -> None:
        if self.identity.uid <= 0 or self.identity.gid <= 0:
            raise GateError("source identity target must be a non-root user")
        try:
            account = pwd.getpwuid(self.identity.uid)
        except KeyError as error:
            raise GateError("source identity target has no local account") from error
        if (
            account.pw_uid != self.identity.uid
            or account.pw_gid != self.identity.gid
            or account.pw_dir != str(self.identity.home)
            or not account.pw_dir.startswith("/")
            or not account.pw_name
            or "\0" in account.pw_name
            or "\0" in account.pw_dir
        ):
            raise GateError("source identity target differs from its local account")
        if (
            not self.git.is_absolute()
            or not self.identity._is_trusted_executable(  # noqa: SLF001
                self.git,
                allowed_owners={0},
            )
        ):
            raise GateError("source identity Git executable is unavailable or untrusted")
        object.__setattr__(
            self,
            "_environment",
            (
                ("HOME", account.pw_dir),
                ("LANG", "C"),
                ("LC_ALL", "C"),
                ("LOGNAME", account.pw_name),
                ("PATH", TRUSTED_SYSTEM_PATH),
                ("USER", account.pw_name),
            ),
        )

    def _git_command(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None,
    ) -> list[str]:
        if timeout <= 0 or timeout > DEFAULT_COMMAND_TIMEOUT_SECONDS:
            raise GateError("source identity command has an invalid execution bound")
        if cwd is None or not cwd.is_absolute():
            raise GateError("source identity command requires an absolute repository")
        if (
            not command
            or command[0] != "git"
            or any(not isinstance(value, str) or "\0" in value for value in command)
        ):
            raise GateError("source identity executor accepts only valid Git commands")
        return [str(self.git), *command[1:]]

    def run(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> CommandResult:
        lowered = self._git_command(command, timeout=timeout, cwd=cwd)
        return self.executor.run_as_identity(
            lowered,
            identity=self.identity,
            timeout=timeout,
            env=dict(self._environment),
            cwd=cwd,
        )

    def run_bytes(
        self,
        command: Sequence[str],
        *,
        timeout: int,
        cwd: Path | None = None,
    ) -> bytes:
        lowered = self._git_command(command, timeout=timeout, cwd=cwd)
        return self.executor.run_bytes_as_identity(
            lowered,
            identity=self.identity,
            timeout=timeout,
            env=dict(self._environment),
            cwd=cwd,
        )


def _stable_node_version(value: str) -> tuple[int, int, int] | None:
    stripped = value.removesuffix("\n")
    match = STABLE_NODE_VERSION.fullmatch(stripped)
    if match is None or value not in (stripped, f"{stripped}\n"):
        return None
    return tuple(
        int(match.group(component))
        for component in ("major", "minor", "patch")
    )


def _selected_source_path_entry(
    selected_directory: Path,
    source_environment: Mapping[str, str] | None,
) -> Path:
    environment = os.environ if source_environment is None else source_environment
    raw_path = environment.get(PACKAGE_BUILD_PATH_ENVIRONMENT)
    if raw_path is None:
        raw_path = environment.get("PATH")
    if not isinstance(raw_path, str):
        raise GateError("package build PATH is missing")
    first_value = raw_path.split(os.pathsep)[0]
    first_entry = Path(first_value)
    if not first_value or not first_entry.is_absolute():
        raise GateError("selected package toolchain PATH entry is invalid")
    try:
        resolved = first_entry.resolve(strict=True)
    except OSError as error:
        raise GateError("selected package toolchain is no longer available") from error
    if resolved != selected_directory:
        raise GateError("selected package toolchain silently changed")
    if first_entry != resolved:
        raise GateError("selected package toolchain PATH entry is an alias")
    return first_entry


def _require_nvm_directory(
    identity: BuildIdentity,
    directory: Path,
    *,
    description: str,
) -> None:
    try:
        metadata = directory.lstat()
        resolved = directory.resolve(strict=True)
    except OSError as error:
        raise GateError(f"{description} is unavailable or untrusted") from error
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or resolved != directory
        or not identity._is_trusted_directory(  # noqa: SLF001
            directory,
            allowed_owners={0, identity.uid},
        )
    ):
        raise GateError(f"{description} is unavailable or untrusted")


def _trusted_nvm_directory_metadata(
    identity: BuildIdentity,
    metadata: os.stat_result,
) -> bool:
    return (
        stat.S_ISDIR(metadata.st_mode)
        and metadata.st_uid in {0, identity.uid}
        and identity._target_writers_are_trusted(metadata)  # noqa: SLF001
        and identity._target_has_permission(  # noqa: SLF001
            metadata,
            stat.S_IXUSR,
            stat.S_IXGRP,
            stat.S_IXOTH,
        )
    )


def _read_validated_nvm_npm_wrapper(
    identity: BuildIdentity,
    version_root: Path,
    npm: Path,
) -> None:
    """Validate the reviewed npm symlink and open its target component-by-component."""

    try:
        npm_metadata = npm.lstat()
        raw_target = os.readlink(npm)
    except OSError as error:
        raise GateError("NVM npm must be a trusted in-root symlink") from error
    if not stat.S_ISLNK(npm_metadata.st_mode):
        raise GateError("NVM npm must be a trusted in-root symlink")

    unnormalized_target = (
        Path(raw_target)
        if Path(raw_target).is_absolute()
        else npm.parent / raw_target
    )
    lexical_target = Path(os.path.normpath(str(unnormalized_target)))
    try:
        relative_target = lexical_target.relative_to(version_root)
    except ValueError as error:
        raise GateError("NVM npm must be a trusted in-root symlink") from error
    if not relative_target.parts:
        raise GateError("NVM npm must be a trusted in-root symlink")

    directory_flags = (
        os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY | os.O_NOFOLLOW
    )
    # O_NONBLOCK makes a concurrently replaced FIFO/device fail validation
    # instead of hanging the host gate before fstat(2) can reject it.
    file_flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
    directory_fd: int | None = None
    target_fd: int | None = None
    try:
        directory_fd = os.open(version_root, directory_flags)
        if not _trusted_nvm_directory_metadata(
            identity,
            os.fstat(directory_fd),
        ):
            raise GateError("NVM npm target path components are untrusted")
        for component in relative_target.parts[:-1]:
            next_fd = os.open(component, directory_flags, dir_fd=directory_fd)
            os.close(directory_fd)
            directory_fd = next_fd
            if not _trusted_nvm_directory_metadata(
                identity,
                os.fstat(directory_fd),
            ):
                raise GateError("NVM npm target path components are untrusted")
        target_fd = os.open(
            relative_target.parts[-1],
            file_flags,
            dir_fd=directory_fd,
        )
        metadata = os.fstat(target_fd)
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid not in {0, identity.uid}
            or not identity._target_writers_are_trusted(metadata)  # noqa: SLF001
            or not identity._target_has_permission(  # noqa: SLF001
                metadata,
                stat.S_IXUSR,
                stat.S_IXGRP,
                stat.S_IXOTH,
            )
        ):
            raise GateError("NVM npm wrapper is unavailable or untrusted")
        if metadata.st_size <= 0 or metadata.st_size > NPM_WRAPPER_MAX_BYTES:
            raise GateError("NVM npm wrapper exceeds its size bound")
        prefix = os.read(target_fd, len(NPM_ENV_NODE_SHEBANG))
        if prefix != NPM_ENV_NODE_SHEBANG:
            raise GateError("NVM npm wrapper has an unexpected shebang")
        if os.readlink(npm) != raw_target or not stat.S_ISLNK(npm.lstat().st_mode):
            raise GateError("NVM npm symlink changed during validation")
    except GateError:
        raise
    except FileNotFoundError as error:
        raise GateError("NVM npm must be a trusted in-root symlink") from error
    except OSError as error:
        if error.errno in {
            getattr(os, "ELOOP", 40),
            getattr(os, "ENOTDIR", 20),
        }:
            raise GateError("NVM npm target path components are untrusted") from error
        raise GateError("NVM npm wrapper is unavailable or untrusted") from error
    finally:
        if target_fd is not None:
            os.close(target_fd)
        if directory_fd is not None:
            os.close(directory_fd)


def _canonical_nvm_version(
    identity: BuildIdentity,
    selected_bin: Path,
    source_bin: Path,
    node: Path,
    npm: Path,
) -> tuple[int, int, int] | None:
    versions_root = identity.home / ".nvm/versions/node"
    selected_is_nvm = selected_bin.is_relative_to(versions_root)
    source_is_nvm = source_bin.is_relative_to(versions_root)
    try:
        resolved_versions_root = versions_root.resolve(strict=True)
    except OSError:
        resolved_versions_root = None
    resolved_is_nvm = (
        resolved_versions_root is not None
        and selected_bin.is_relative_to(resolved_versions_root)
    )
    if not (selected_is_nvm or source_is_nvm or resolved_is_nvm):
        return None

    canonical_bin = source_bin if source_is_nvm else selected_bin
    version_root = canonical_bin.parent
    if canonical_bin.name != "bin" or version_root.parent != versions_root:
        raise GateError("selected NVM toolchain path is noncanonical")
    version = _stable_node_version(version_root.name)
    if version is None or version[0] not in SUPPORTED_NODE_MAJORS:
        raise GateError("selected NVM toolchain version is unsupported")

    for directory, description in (
        (identity.home, "NVM home"),
        (identity.home / ".nvm", "NVM root"),
        (identity.home / ".nvm/versions", "NVM versions root"),
        (versions_root, "NVM Node versions root"),
        (version_root, "NVM version root"),
        (canonical_bin, "NVM bin directory"),
    ):
        _require_nvm_directory(identity, directory, description=description)
    if selected_bin != canonical_bin or node != canonical_bin / "node":
        raise GateError("selected NVM Node executable is noncanonical")
    if npm != canonical_bin / "npm":
        raise GateError("selected NVM npm executable is noncanonical")

    try:
        node_metadata = node.lstat()
    except OSError as error:
        raise GateError("NVM node must be a regular executable") from error
    if (
        not stat.S_ISREG(node_metadata.st_mode)
        or stat.S_ISLNK(node_metadata.st_mode)
        or not identity._is_trusted_executable(  # noqa: SLF001
            node,
            allowed_owners={0, identity.uid},
        )
    ):
        raise GateError("NVM node must be a regular executable")
    _read_validated_nvm_npm_wrapper(identity, version_root, npm)
    return version


def resolve_package_toolchain(
    identity: BuildIdentity,
    executor: CommandExecutor,
    *,
    source_environment: Mapping[str, str] | None = None,
) -> PackageToolchain:
    """Resolve one coherent pair and revalidate its actual Node runtime once."""

    trusted_path = identity._trusted_path(source_environment)  # noqa: SLF001
    selected_bin = trusted_path[0]
    source_bin = _selected_source_path_entry(
        selected_bin,
        source_environment,
    )
    node = selected_bin / "node"
    npm = selected_bin / "npm"
    node_is_trusted = identity._is_trusted_executable(  # noqa: SLF001
        node,
        allowed_owners={0, identity.uid},
    )
    npm_is_trusted = identity._is_trusted_executable(  # noqa: SLF001
        npm,
        allowed_owners={0, identity.uid},
    )
    nvm_dangling_npm = (
        selected_bin.is_relative_to(identity.home / ".nvm/versions/node")
        and node_is_trusted
        and npm.is_symlink()
    )
    if not ((node_is_trusted and npm_is_trusted) or nvm_dangling_npm):
        raise GateError(
            "selected package toolchain no longer has coherent trusted node and npm"
        )
    expected_nvm_version = _canonical_nvm_version(
        identity,
        selected_bin,
        source_bin,
        node,
        npm,
    )
    git = identity.resolve_system_executable("git")
    toolchain = PackageToolchain(
        node,
        npm,
        git,
        trusted_path,
        (0, 0, 0),
    )
    result = executor.run_probe(
        identity.command(
            (str(node), "--version"),
            source_environment=toolchain.source_environment(),
        ),
        timeout=NODE_PROBE_TIMEOUT_SECONDS,
        output_limit=NODE_PROBE_OUTPUT_LIMIT_BYTES,
    )
    if result.stderr:
        raise GateError("node version probe emitted unexpected stderr")
    runtime_version = _stable_node_version(result.stdout)
    if runtime_version is None or runtime_version[0] not in SUPPORTED_NODE_MAJORS:
        raise GateError("node version probe returned an unsupported stable version")
    if expected_nvm_version is not None and runtime_version != expected_nvm_version:
        raise GateError("node runtime does not match selected NVM version")
    return dataclasses.replace(toolchain, version=runtime_version)


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

    rendered: list[str] = []
    for argument in command:
        value = str(argument)
        if value.startswith("HOME="):
            value = "HOME=<redacted>"
        elif value.startswith("PATH="):
            value = "PATH=<redacted>"
        rendered.append(value)
    return " ".join(rendered)


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


def validate_quickstart_output(
    output: str,
    *,
    language: str,
    expect_auth_failure: bool,
) -> None:
    """Require exact, non-label-derived completion evidence from one variant."""

    if not language or re.fullmatch(r"[a-z-]+", language) is None:
        raise GateError("quick-start language label is invalid")
    lines = output.splitlines()
    terminal = (
        f"quickstart-ok language={language} "
        f"mode={'auth-failure' if expect_auth_failure else 'success'}"
    )
    if expect_auth_failure:
        if lines != [terminal]:
            raise GateError(
                f"{language} authentication probe emitted unexpected completion evidence"
            )
        return
    expected = [
        f"quickstart-ok language={language} behavior={behavior}"
        for behavior in REQUIRED_BEHAVIORS
    ]
    expected.append(terminal)
    if lines != expected:
        raise GateError(
            f"{language} did not execute every required behavior exactly once "
            "in canonical order"
        )


def parse_loopback_port(value: str) -> int:
    """Parse Docker's one expected dynamically published IPv4 binding."""

    match = LOOPBACK_PORT.fullmatch(value.strip())
    if match is None:
        raise GateError(f"Docker returned an unexpected API binding: {value!r}")
    port = int(match.group(1))
    if not 1 <= port <= 65_535:
        raise GateError("Docker returned an invalid dynamic API port")
    return port


def _image_path(value: str, *, description: str) -> PurePosixPath:
    """Parse one canonical absolute image path without host-path ambiguity."""

    if "\0" in value or "\\" in value:
        raise GateError(f"{description} contains an unsafe image path")
    path = PurePosixPath(value)
    if (
        not path.is_absolute()
        or path.as_posix() != value
        or any(part in ("", ".", "..") for part in path.parts[1:])
    ):
        raise GateError(f"{description} contains a noncanonical image path")
    return path


def _copied_image_path(root: Path, absolute_path: str) -> Path:
    path = _image_path(absolute_path, description="runtime evidence")
    return root.joinpath(*path.parts[1:])


def _is_image_path_within(path: PurePosixPath, scope: PurePosixPath) -> bool:
    return path.parts[: len(scope.parts)] == scope.parts


def _parse_first_party_manifest(raw: bytes) -> dict[str, str]:
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise GateError("first-party manifest is not UTF-8") from error
    if not text.endswith("\n"):
        raise GateError("first-party manifest is not newline terminated")
    lines = text.splitlines()
    if not lines or lines[0] != (
        "path\tsha256\tlicense_expression\tlicense_evidence"
    ):
        raise GateError("first-party manifest has an invalid header")
    entries: dict[str, str] = {}
    ordered_paths: list[str] = []
    evidence_scopes = tuple(
        _image_path(path, description="runtime evidence scope")
        for path in RUNTIME_EVIDENCE_PATHS
    )
    for line in lines[1:]:
        fields = line.split("\t")
        if len(fields) != 4:
            raise GateError("first-party manifest has a malformed record")
        path_text, digest, license_expression, evidence_text = fields
        path = _image_path(path_text, description="first-party manifest")
        evidence = _image_path(
            evidence_text,
            description="first-party license evidence",
        )
        del evidence
        if (
            re.fullmatch(r"[0-9a-f]{64}", digest) is None
            or not license_expression
            or path_text in entries
            or not any(
                _is_image_path_within(path, scope) for scope in evidence_scopes
            )
        ):
            raise GateError("first-party manifest has an invalid record")
        entries[path_text] = digest
        ordered_paths.append(path_text)
    if not entries or ordered_paths != sorted(ordered_paths):
        raise GateError("first-party manifest is empty or nondeterministically ordered")
    return entries


def _validate_symlink_target(path: PurePosixPath, target: str) -> None:
    if "\0" in target or "\\" in target or target == "":
        raise GateError(f"critical runtime symlink has an unsafe target: {path}")
    target_path = PurePosixPath(target)
    if target_path.is_absolute():
        _image_path(target, description="critical runtime symlink")
        return

    resolved = list(path.parent.parts[1:])
    for part in target_path.parts:
        if part in ("", "."):
            continue
        if part == "..":
            if not resolved:
                raise GateError(
                    f"critical runtime symlink escapes the image root: {path}"
                )
            resolved.pop()
        else:
            resolved.append(part)


def _critical_tree_inventory(
    root: Path,
    absolute_path: str,
    *,
    excluded_paths: Sequence[str] = (),
) -> dict[str, tuple[str, int, str]]:
    """Fingerprint one copied image tree without following symlinks."""

    start_image_path = _image_path(
        absolute_path,
        description="critical runtime path",
    )
    start = _copied_image_path(root, absolute_path)
    excluded = tuple(
        _image_path(path, description="critical runtime exclusion")
        for path in excluded_paths
    )
    if not start.exists() and not start.is_symlink():
        raise GateError(f"copied critical runtime path is missing: {absolute_path}")

    inventory: dict[str, tuple[str, int, str]] = {}
    pending = [(start_image_path, start)]
    while pending:
        image_path, host_path = pending.pop()
        if any(_is_image_path_within(image_path, prefix) for prefix in excluded):
            continue
        metadata = host_path.lstat()
        mode = stat.S_IMODE(metadata.st_mode)
        key = image_path.as_posix()
        if stat.S_ISLNK(metadata.st_mode):
            target = os.readlink(host_path)
            _validate_symlink_target(image_path, target)
            inventory[key] = ("symlink", mode, target)
        elif stat.S_ISREG(metadata.st_mode):
            inventory[key] = ("file", mode, file_sha256(host_path))
        elif stat.S_ISDIR(metadata.st_mode):
            inventory[key] = ("directory", mode, "")
            for child in sorted(host_path.iterdir(), reverse=True):
                pending.append((image_path / child.name, child))
        else:
            raise GateError(f"critical runtime tree contains a special file: {key}")
    return inventory


def validate_copied_runtime_parity(
    production_root: Path,
    fixture_root: Path,
    *,
    critical_paths: Sequence[str] = CRITICAL_RUNTIME_PATHS,
) -> None:
    """Validate manifest truth and exact inherited runtime parity host-side."""

    production_manifest = _copied_image_path(
        production_root,
        FIRST_PARTY_MANIFEST_PATH,
    )
    fixture_manifest = _copied_image_path(
        fixture_root,
        FIRST_PARTY_MANIFEST_PATH,
    )
    if (
        production_manifest.is_symlink()
        or fixture_manifest.is_symlink()
        or not production_manifest.is_file()
        or not fixture_manifest.is_file()
    ):
        raise GateError("first-party manifest is missing or not a regular file")
    production_raw = production_manifest.read_bytes()
    fixture_raw = fixture_manifest.read_bytes()
    if production_raw != fixture_raw:
        raise GateError("fixture first-party manifest differs from production")
    entries = _parse_first_party_manifest(production_raw)
    for path, expected_digest in entries.items():
        for label, root in (
            ("production", production_root),
            ("fixture", fixture_root),
        ):
            copied = _copied_image_path(root, path)
            try:
                actual_digest = file_sha256(copied)
            except GateError as error:
                raise GateError(
                    f"{label} first-party manifest path is not a regular file: {path}"
                ) from error
            if actual_digest != expected_digest:
                raise GateError(
                    f"{label} first-party manifest hash mismatch: {path}"
                )

    for path in critical_paths:
        exclusions = (FIXTURE_IMAGE_PATH,) if path == "/usr/share/xenoteer" else ()
        production_inventory = _critical_tree_inventory(
            production_root,
            path,
            excluded_paths=exclusions,
        )
        fixture_inventory = _critical_tree_inventory(
            fixture_root,
            path,
            excluded_paths=exclusions,
        )
        if production_inventory != fixture_inventory:
            raise GateError(f"fixture changed the critical runtime tree: {path}")


def _repository_fixture_inventory(
    root: Path,
) -> dict[str, tuple[str, int, str]]:
    if root.is_symlink() or not root.is_dir():
        raise GateError("desktop fixture source tree is missing or unsafe")
    inventory: dict[str, tuple[str, int, str]] = {}
    pending = [root]
    while pending:
        current = pending.pop()
        relative_directory = current.relative_to(root).as_posix()
        inventory[relative_directory] = (
            "directory",
            stat.S_IMODE(current.lstat().st_mode),
            "",
        )
        for child in sorted(current.iterdir(), reverse=True):
            metadata = child.lstat()
            relative = child.relative_to(root).as_posix()
            if stat.S_ISDIR(metadata.st_mode):
                pending.append(child)
            elif stat.S_ISREG(metadata.st_mode):
                inventory[relative] = (
                    "file",
                    stat.S_IMODE(metadata.st_mode),
                    file_sha256(child),
                )
            else:
                raise GateError(
                    f"desktop fixture source contains a symlink or special file: "
                    f"{relative}"
                )
    if not inventory:
        raise GateError("desktop fixture source tree is empty")
    return inventory


def validate_fixture_repository_inputs(
    fixture_root: Path,
    repository_root: Path,
    image: ExactFixtureImage | None = None,
) -> None:
    """Bind fixture-only image inputs to the source tree used for packaging."""

    copied_fixtures = _copied_image_path(fixture_root, FIXTURE_IMAGE_PATH)
    repository_fixtures = repository_root / FIXTURE_REPOSITORY_PATH
    if _repository_fixture_inventory(
        copied_fixtures
    ) != _repository_fixture_inventory(repository_fixtures):
        raise GateError("desktop fixture image sources differ from the repository")

    copied_lock = _copied_image_path(
        fixture_root,
        FIXTURE_ARTIFACT_LOCK_IMAGE_PATH,
    )
    repository_lock = repository_root / FIXTURE_ARTIFACT_LOCK_REPOSITORY_PATH
    if (
        file_sha256(copied_lock) != file_sha256(repository_lock)
        or copied_lock.read_bytes() != repository_lock.read_bytes()
    ):
        raise GateError("desktop fixture artifact lock differs from the repository")
    if image is not None:
        artifact_lock = read_fixture_artifact_lock(repository_lock)
        if (
            artifact_lock.electron_version != image.electron_version
            or artifact_lock.electron_linux_x64_sha256
            != image.electron_linux_x64_sha256
        ):
            raise GateError("desktop fixture labels differ from the artifact lock")


def _copy_stopped_image_runtime(
    executor: CommandExecutor,
    image_id: str,
    destination: Path,
) -> None:
    """Copy audited paths from a newly created container without executing it."""

    image_id = validate_image_id(image_id)
    destination.mkdir(parents=True, exist_ok=False)
    container_name = f"xenoteer-runtime-parity-{secrets.token_hex(12)}"
    with ContainerGuard(executor, container_name) as guard:
        created = executor.run(
            ["docker", "create", "--name", container_name, image_id],
            timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            check=False,
        )
        if created.returncode != 0:
            raise GateError("could not create stopped runtime-evidence container")
        guard.mark_created()
        copied_image_id = _docker_inspect(
            executor,
            ["inspect", container_name, "--format", "{{.Image}}"],
        )
        running = _docker_inspect(
            executor,
            ["inspect", container_name, "--format", "{{.State.Running}}"],
        )
        if copied_image_id != image_id or running != "false":
            raise GateError("runtime-evidence container identity or state changed")
        for absolute_path in RUNTIME_EVIDENCE_PATHS:
            target = _copied_image_path(destination, absolute_path)
            target.parent.mkdir(parents=True, exist_ok=True)
            copied = executor.run(
                [
                    "docker",
                    "cp",
                    f"{container_name}:{absolute_path}",
                    str(target),
                ],
                timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
                check=False,
            )
            if copied.returncode != 0:
                raise GateError(
                    f"could not copy required runtime evidence: {absolute_path}"
                )
        final_image_id = _docker_inspect(
            executor,
            ["inspect", container_name, "--format", "{{.Image}}"],
        )
        final_running = _docker_inspect(
            executor,
            ["inspect", container_name, "--format", "{{.State.Running}}"],
        )
        if final_image_id != image_id or final_running != "false":
            raise GateError(
                "runtime-evidence container changed identity or ran during collection"
            )


def validate_fixture_runtime_parity(
    executor: CommandExecutor,
    image: ExactFixtureImage,
    destination: Path,
    repository_root: Path,
) -> None:
    """Copy stopped rootfs evidence and prove production/fixture parity."""

    destination.mkdir(parents=True, exist_ok=False)
    production_root = destination / "production"
    fixture_root = destination / "fixture"
    _copy_stopped_image_runtime(executor, image.production_id, production_root)
    _copy_stopped_image_runtime(executor, image.fixture_id, fixture_root)
    validate_copied_runtime_parity(production_root, fixture_root)
    validate_fixture_repository_inputs(fixture_root, repository_root, image)


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
    toolchain: PackageToolchain,
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

    source_environment = toolchain.source_environment()
    cargo_binary = identity.resolve_executable(
        "cargo",
        source_environment=source_environment,
    )
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
                ],
                source_environment=source_environment,
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
        identity.command(
            [str(toolchain.npm), "run", "build"],
            source_environment=source_environment,
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=repository_root / "packages" / "typescript",
    )
    npm_output = executor.run(
        identity.command(
            [
                str(toolchain.npm),
                "pack",
                "--json",
                "--pack-destination",
                str(npm_stage),
            ],
            source_environment=source_environment,
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
            ],
            source_environment=source_environment,
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
            ],
            source_environment=source_environment,
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
    toolchain: PackageToolchain,
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
        'serde_json = "=1.0.151"\n'
        'tokio = { version = "=1.53.1", features = ["macros", "rt-multi-thread", "time"] }\n\n'
        "[patch.crates-io]\n"
        f"xenoteer-protocol = {{ path = {json.dumps(str(protocol_root))} }}\n\n"
        "[workspace]\n",
    )
    (rust_consumer / "src").mkdir()
    rust_example = sdk_root / "examples" / "phase6_behaviors.rs"
    if not rust_example.is_file():
        raise GateError("staged Rust crate omitted examples/phase6_behaviors.rs")
    _write_text(
        rust_consumer / "src" / "main.rs",
        rust_example.read_text(encoding="utf-8"),
    )

    typescript_root = installs / "typescript"
    _write_text(
        typescript_root / "package.json",
        '{"name":"xenoteer-public-quickstart","private":true,"type":"module"}\n',
    )

    wheel_root = installs / "python-wheel"
    sdist_root = installs / "python-sdist"
    for root in (wheel_root, sdist_root):
        root.mkdir()
    chown_tree(workspace, identity)

    source_environment = toolchain.source_environment()
    cargo_binary = identity.resolve_executable(
        "cargo",
        source_environment=source_environment,
    )
    rust_target = repository_root / "target" / "phase6-public-quickstarts"
    executor.run(
        identity.command(
            [
                str(cargo_binary),
                "generate-lockfile",
                "--offline",
                "--manifest-path",
                str(rust_consumer / "Cargo.toml"),
            ],
            source_environment=source_environment,
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=rust_consumer,
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
            ],
            source_environment=source_environment,
        ),
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        cwd=rust_consumer,
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
            ],
            source_environment=source_environment,
        ),
        timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
        cwd=rust_consumer,
    )

    executor.run(
        identity.command(
            [
                str(toolchain.npm),
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--package-lock=false",
                str(artifacts.npm_tarball),
            ],
            source_environment=source_environment,
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
            identity.command(
                install_command,
                source_environment=source_environment,
            ),
            timeout=PACKAGE_COMMAND_TIMEOUT_SECONDS,
            cwd=root,
        )
        python_commands.append(
            (sys.executable, "-m", "xenoteer.examples.phase6_behaviors")
        )
        python_roots.append(site)

    rust_binary = rust_target / "debug" / "xenoteer-public-quickstart"
    if not rust_binary.is_file():
        raise GateError("staged Rust quick-start build omitted its binary")
    typescript_example = (
        typescript_root
        / "node_modules"
        / "@xenoteer"
        / "sdk"
        / "examples"
        / "phase6-behaviors.mjs"
    )
    if not typescript_example.is_file():
        raise GateError("staged npm tarball omitted examples/phase6-behaviors.mjs")
    return InstalledQuickstarts(
        (str(rust_binary),),
        rust_artifacts,
        (str(toolchain.node), str(typescript_example)),
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
) -> ExactFixtureImage:
    """Resolve one derived fixture and its production base exactly once."""

    fixture_id = validate_image_id(
        _docker_inspect(
            executor,
            ["image", "inspect", image_reference, "--format", "{{.Id}}"],
        )
    )
    fixture_kind = _docker_inspect(
        executor,
        [
            "image",
            "inspect",
            fixture_id,
            "--format",
            '{{index .Config.Labels "com.aeor.xenoteer.fixture"}}',
        ],
    )
    if fixture_kind != "phase-2-desktop-apps":
        raise GateError("release candidate is not the desktop-app fixture image")
    production_id = validate_image_id(
        _docker_inspect(
            executor,
            [
                "image",
                "inspect",
                fixture_id,
                "--format",
                '{{index .Config.Labels "com.aeor.xenoteer.fixture.base-image-id"}}',
            ],
        )
    )
    inspected = executor.run(
        ["docker", "image", "inspect", production_id, fixture_id],
        timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
    ).stdout
    return validate_fixture_image_metadata(fixture_id, production_id, inspected)


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
    identity: BuildIdentity,
    name: str,
    command: Sequence[str],
    installed_root: Path,
    api_base: str,
    token: str,
    expect_auth_failure: bool,
    forbidden_tokens: Sequence[str],
    source_environment: Mapping[str, str] | None = None,
) -> None:
    """Run one installed consumer with bounded typed-auth expectations."""

    runtime_root = identity._trusted_installed_root(installed_root)
    home = identity._trusted_home()
    trusted_path = identity._trusted_path(source_environment)
    environment = {
        "HOME": str(home),
        "PATH": os.pathsep.join(str(directory) for directory in trusted_path),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "XENOTEER_API_BASE": api_base,
        "XENOTEER_TOKEN": token,
        "XENOTEER_EXPECTED_INSTALL_ROOT": str(runtime_root),
        "XENOTEER_EXPECT_AUTH_FAILURE": "1" if expect_auth_failure else "0",
        "XENOTEER_QUICKSTART_LANGUAGE": name,
        "PYTHONNOUSERSITE": "1",
        "RUST_BACKTRACE": "0",
    }
    if name.startswith("python-"):
        environment["PYTHONPATH"] = str(runtime_root)
    runtime_command = identity.runtime_command(
        command,
        source_environment=source_environment,
    )
    if any(
        secret and any(secret in argument for argument in runtime_command)
        for secret in forbidden_tokens
    ):
        raise GateError(f"{name} quick-start command contains a bearer canary")
    result = executor.run_as_identity(
        runtime_command,
        identity=identity,
        timeout=QUICKSTART_COMMAND_TIMEOUT_SECONDS,
        check=False,
        cwd=runtime_root,
        env=environment,
    )
    combined = result.stdout + result.stderr
    if any(secret in combined for secret in forbidden_tokens):
        raise GateError(f"{name} quick-start exposed a bearer canary")
    mode = "auth-failure" if expect_auth_failure else "success"
    if result.returncode != 0:
        raise GateError(
            f"{name} {mode} quick-start failed: "
            f"{safe_diagnostic(combined) or 'no safe diagnostic'}"
        )
    validate_quickstart_output(
        result.stdout,
        language=name,
        expect_auth_failure=expect_auth_failure,
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
    image: ExactFixtureImage,
    executor: CommandExecutor,
    identity: BuildIdentity,
    toolchain: PackageToolchain,
    source_executor: SourceIdentityExecutor,
) -> None:
    """Exercise every archive variant in fresh exact-fixture state."""

    token = "PHASE6_PUBLIC_QUICKSTART_TOKEN_" + secrets.token_hex(24)
    wrong_token = "PHASE6_PUBLIC_QUICKSTART_WRONG_" + secrets.token_hex(24)
    token_file = workspace / "api-token"
    token_file.write_text(token, encoding="ascii")
    token_file.chmod(0o400)
    if not _root_owned_token_supported(executor):
        raise GateError(
            "run as root or use rootless Docker so the token maps to container UID 0"
        )

    for name, command, installed_root in installed.variants():
        container_name = (
            f"xenoteer-phase6-{name}-{os.getpid()}-{secrets.token_hex(4)}"
        )
        with ContainerGuard(executor, container_name) as guard:
            # Arm cleanup before Docker can create the named container and then
            # lose its response or exceed the subprocess deadline.
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
                    "--tmpfs",
                    "/run/xenoteer/artifacts:rw,noexec,nosuid,nodev,"
                    "size=512m,mode=0700,uid=1001,gid=1001",
                    "--log-driver",
                    "json-file",
                    "--log-opt",
                    "max-size=2m",
                    "--log-opt",
                    "max-file=1",
                    "--publish",
                    "127.0.0.1::8080",
                    "--env",
                    "DESKTOP_PROFILE=bare",
                    "--env",
                    "XENOTEER__VIEWER__ENABLED=true",
                    "--env",
                    'XENOTEER__VIEWER__ALLOWED_ORIGINS=["https://viewer.example"]',
                    "--volume",
                    f"{token_file}:/run/secrets/xenoteer_api_token:ro",
                    image.fixture_id,
                ],
                timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            )
            running_image = validate_image_id(
                _docker_inspect(
                    executor,
                    ["inspect", container_name, "--format", "{{.Image}}"],
                )
            )
            if running_image != image.fixture_id:
                raise GateError(
                    "Docker container did not retain the resolved fixture image ID"
                )
            port = parse_loopback_port(
                _docker_inspect(executor, ["port", container_name, "8080/tcp"])
            )
            api_base = f"http://127.0.0.1:{port}"
            wait_until_ready(executor, container_name, api_base)
            fixture_path = (
                "/usr/share/xenoteer/fixtures/desktop-apps/gtk3-fixture.py"
            )
            executor.run(
                ["docker", "exec", container_name, "test", "-x", fixture_path],
                timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            )
            executor.run(
                [
                    "docker",
                    "exec",
                    "--detach",
                    "--user",
                    "1000",
                    container_name,
                    "/command/s6-envdir",
                    "-f",
                    "-L",
                    "/run/xenoteer/env",
                    fixture_path,
                ],
                timeout=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            )
            run_one_quickstart(
                executor,
                identity=identity,
                name=name,
                command=command,
                installed_root=installed_root,
                api_base=api_base,
                token=wrong_token,
                expect_auth_failure=True,
                forbidden_tokens=(token, wrong_token),
                source_environment=toolchain.source_environment(),
            )
            run_one_quickstart(
                executor,
                identity=identity,
                name=name,
                command=command,
                installed_root=installed_root,
                api_base=api_base,
                token=token,
                expect_auth_failure=False,
                forbidden_tokens=(token, wrong_token),
                source_environment=toolchain.source_environment(),
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
                raise GateError(
                    f"{name} fixture container did not stop within the cleanup bound"
                )
            exit_code = _docker_inspect(
                executor,
                ["inspect", container_name, "--format", "{{.State.ExitCode}}"],
            )
            if exit_code != "0":
                raise GateError(
                    f"{name} fixture container returned nonzero after examples: "
                    f"{exit_code}"
                )
            assert_container_logs_safe(
                executor,
                container_name,
                (token, wrong_token),
            )

    final_source_tree_sha256 = current_source_tree_hash(
        repository_root,
        source_executor,
    )
    if final_source_tree_sha256 != artifacts.source_tree_sha256:
        raise GateError("source tree changed while the public quick-start gate was running")


def qualify(image_reference: str) -> dict[str, str]:
    """Run the complete gate and return exact identities only after success."""

    reject_daemon_overrides(os.environ)
    repository_root = Path(__file__).resolve().parents[2]
    executor = CommandExecutor()
    identity = BuildIdentity.current()
    toolchain = resolve_package_toolchain(identity, executor)
    source_executor = SourceIdentityExecutor(identity, executor, toolchain.git)
    image = resolve_exact_image(executor, image_reference)
    current_hash = current_source_tree_hash(repository_root, source_executor)
    if current_hash != image.source_tree_sha256:
        raise GateError(
            "production image source identity differs from the current package tree"
        )

    with tempfile.TemporaryDirectory(
        prefix="xenoteer-phase6-public-quickstarts-"
    ) as temporary:
        workspace = Path(temporary)
        chown_tree(workspace, identity)
        validate_fixture_runtime_parity(
            executor,
            image,
            workspace / "runtime-parity",
            repository_root,
        )
        artifacts = stage_public_artifacts(
            repository_root,
            workspace,
            current_hash,
            executor,
            identity,
            toolchain,
        )
        after_staging_hash = current_source_tree_hash(
            repository_root,
            source_executor,
        )
        if after_staging_hash != image.source_tree_sha256:
            raise GateError(
                "source tree changed after the release-candidate image or during packaging"
            )
        installed = prepare_installed_quickstarts(
            repository_root,
            workspace,
            artifacts,
            executor,
            identity,
            toolchain,
        )
        before_live_hash = current_source_tree_hash(
            repository_root,
            source_executor,
        )
        if before_live_hash != image.source_tree_sha256:
            raise GateError(
                "source tree changed while installed quick-start consumers were prepared"
            )
        run_live_gate(
            repository_root,
            workspace,
            artifacts,
            installed,
            image,
            executor,
            identity,
            toolchain,
            source_executor,
        )
        return {
            "fixture_image": image.fixture_id,
            "production_image": image.production_id,
            "source_tree": image.source_tree_sha256,
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
