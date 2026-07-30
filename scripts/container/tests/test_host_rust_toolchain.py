#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Bounded regression tests for container host-runner Rust discovery."""

from __future__ import annotations

import os
import pwd
import shlex
import shutil
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
EVENT_FLOOD_RUNNER = REPO_ROOT / "scripts/container/test-phase4-event-flood.sh"
PHASE3_RUNNER = REPO_ROOT / "scripts/container/test-phase3-control-plane.sh"
VIEWER_DENIAL_RUNNER = REPO_ROOT / "scripts/container/test-viewer-denial.sh"
PUBLIC_QUICKSTARTS = REPO_ROOT / "scripts/sdk/public_quickstarts.py"


def write_executable(path: Path, contents: str) -> None:
    path.write_text(textwrap.dedent(contents))
    path.chmod(0o755)


def create_rustup_proxies(home: Path, *, host: str = "x86_64-unknown-linux-gnu") -> None:
    cargo_bin = home / ".cargo/bin"
    cargo_bin.mkdir(parents=True)
    rustup = cargo_bin / "rustup"
    write_executable(
        rustup,
        f"""\
        #!/bin/sh
        case "${{0##*/}}" in
          rustc)
            printf '%s\\n' 'rustc 1.0.0 (test)' 'host: {host}'
            ;;
          cargo)
            printf '%s\\n' 'cargo 1.0.0 (test)'
            ;;
          *)
            exit 96
            ;;
        esac
        """,
    )
    (cargo_bin / "cargo").symlink_to("rustup")
    (cargo_bin / "rustc").symlink_to("rustup")


def secure_system_path(*prefixes: Path) -> str:
    return os.pathsep.join(
        (*(str(prefix) for prefix in prefixes), "/usr/sbin", "/usr/bin", "/sbin", "/bin")
    )


class EventFloodOrderingTests(unittest.TestCase):
    def test_secure_path_resolves_real_invoking_user_before_ambient_rust_checks(
        self,
    ) -> None:
        invoking_uid = os.getuid()
        if invoking_uid == 0:
            self.skipTest(
                "real invoking-user integration needs a non-root account; "
                "mocked identity tests cover root execution"
            )
        invoking_user = pwd.getpwuid(invoking_uid)
        for tool in ("cargo", "rustc"):
            self.assertTrue(
                (Path(invoking_user.pw_dir) / ".cargo/bin" / tool).is_file(),
                f"real invoking user must provide the rustup {tool} proxy",
            )

        with tempfile.TemporaryDirectory(
            prefix="xenoteer-host-rust-ordering-"
        ) as temporary:
            temporary_path = Path(temporary)
            fake_bin = temporary_path / "bin"
            fake_bin.mkdir()
            fake_docker = fake_bin / "docker"
            write_executable(
                fake_docker,
                """\
                #!/bin/sh
                case "$*" in
                  *'{{.Id}}'*) printf '%s\n' 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
                  *'{{.Os}}'*) printf '%s\n' 'not-linux' ;;
                  *'{{.Architecture}}'*) printf '%s\n' 'amd64' ;;
                  *) exit 97 ;;
                esac
                """,
            )
            secure_path = secure_system_path(fake_bin)
            self.assertIsNone(shutil.which("cargo", path=secure_path))
            self.assertIsNone(shutil.which("rustc", path=secure_path))

            environment = {
                "HOME": "/root",
                "LANG": "C.UTF-8",
                "LC_ALL": "C.UTF-8",
                "PATH": secure_path,
                "SUDO_GID": str(invoking_user.pw_gid),
                "SUDO_UID": str(invoking_uid),
                "SUDO_USER": invoking_user.pw_name,
                "TMPDIR": str(temporary_path),
            }
            completed = subprocess.run(
                [str(EVENT_FLOOD_RUNNER), "fixture:ordering-regression"],
                cwd=REPO_ROOT,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=8,
                check=False,
            )

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("fixture/image platform mismatch", completed.stderr)
        self.assertNotIn("required command is unavailable: cargo", completed.stderr)
        self.assertNotIn("required command is unavailable: rustc", completed.stderr)


class HostRustToolchainTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(
            dir=Path.home(), prefix=".host-rust-toolchain-test-"
        )
        self.root = Path(self.temporary.name)
        self.command_bin = self.root / "commands"
        self.command_bin.mkdir()
        self.home = self.root / "home"
        self.home.mkdir()
        self.uid = os.getuid()
        self.gid = os.getgid()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def install_getent(
        self,
        *,
        uid: int | None = None,
        gid: int | None = None,
        home: Path | None = None,
        exit_status: int = 0,
    ) -> None:
        resolved_uid = self.uid if uid is None else uid
        resolved_gid = self.gid if gid is None else gid
        resolved_home = self.home if home is None else home
        write_executable(
            self.command_bin / "getent",
            f"""\
            #!/bin/sh
            if [ "$1" != passwd ] || [ "$2" != "{resolved_uid}" ]; then
              exit 98
            fi
            if [ "{exit_status}" -ne 0 ]; then
              exit "{exit_status}"
            fi
            printf '%s\\n' 'builder:x:{resolved_uid}:{resolved_gid}:Builder:{resolved_home}:/bin/sh'
            """,
        )

    def run_self_test(
        self,
        *,
        extra_environment: dict[str, str] | None = None,
        path: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = {
            "HOME": "/root",
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "PATH": path or secure_system_path(self.command_bin),
        }
        if extra_environment:
            environment.update(extra_environment)
        return subprocess.run(
            [str(EVENT_FLOOD_RUNNER), "--self-test-host-rust-toolchain"],
            cwd=REPO_ROOT,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=8,
            check=False,
        )

    def test_self_test_supports_non_sudo_invocation_and_relative_rustup_proxies(
        self,
    ) -> None:
        self.install_getent()
        create_rustup_proxies(self.home)

        completed = self.run_self_test()

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            f"host=x86_64-unknown-linux-gnu uid={self.uid}\n",
        )
        self.assertEqual(completed.stderr, "")

    def test_secure_path_ignores_stale_ambient_rust_tools_under_sudo(self) -> None:
        self.install_getent()
        create_rustup_proxies(self.home)
        stale_bin = self.root / "stale"
        stale_bin.mkdir()
        for tool in ("cargo", "rustc"):
            write_executable(
                stale_bin / tool,
                """\
                #!/bin/sh
                printf '%s\n' 'stale ambient Rust tool executed' >&2
                exit 95
                """,
            )

        completed = self.run_self_test(
            extra_environment={
                "SUDO_GID": str(self.gid),
                "SUDO_UID": str(self.uid),
                "SUDO_USER": "builder",
            },
            path=secure_system_path(stale_bin, self.command_bin),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn("stale", completed.stderr)

    def test_rust_identity_boundary_drops_overrides_and_secret_canaries(self) -> None:
        self.install_getent()
        create_rustup_proxies(self.home)
        rustup = self.home / ".cargo/bin/rustup"
        write_executable(
            rustup,
            """\
            #!/bin/sh
            [ "$RUSTC" = "$HOME/.cargo/bin/rustc" ] || {
              printf '%s\n' 'RUSTC did not name the validated rustc proxy' >&2
              exit 91
            }
            for name in RUSTUP_TOOLCHAIN RUSTC_WRAPPER RUSTFLAGS \
              CARGO_TARGET_DIR XENOTEER_TOKEN HOST_RUST_SECRET_CANARY; do
              eval 'value=${'"$name"'+set}'
              if [ "$value" = set ]; then
                printf 'forbidden environment variable crossed boundary: %s\n' \
                  "$name" >&2
                exit 90
              fi
            done
            [ "$CARGO_HOME" = "$HOME/.cargo" ] || exit 89
            [ "$RUSTUP_HOME" = "$HOME/.rustup" ] || exit 88
            [ "$LANG" = C.UTF-8 ] || exit 87
            [ "$LC_ALL" = C.UTF-8 ] || exit 86
            case "${0##*/}" in
              rustc)
                printf '%s\n' 'rustc 1.0.0 (test)' \
                  'host: x86_64-unknown-linux-gnu'
                ;;
              cargo)
                printf '%s\n' 'cargo 1.0.0 (test)'
                ;;
              *)
                exit 85
                ;;
            esac
            """,
        )
        canary = "HOST_RUST_BOUNDARY_SECRET_0123456789"

        completed = self.run_self_test(
            extra_environment={
                "CARGO_TARGET_DIR": "/untrusted/target",
                "HOST_RUST_SECRET_CANARY": canary,
                "RUSTC_WRAPPER": "/untrusted/wrapper",
                "RUSTC": "/untrusted/ambient-rustc",
                "RUSTFLAGS": "--cfg untrusted_override",
                "RUSTUP_TOOLCHAIN": "untrusted-toolchain",
                "XENOTEER_TOKEN": canary,
            }
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn(canary, completed.stdout)
        self.assertNotIn(canary, completed.stderr)

    def test_custom_absolute_toolchain_gives_cargo_the_validated_rustc(self) -> None:
        self.install_getent()
        custom_bin = self.root / "custom-rust"
        custom_bin.mkdir()
        rustc = custom_bin / "rustc"
        cargo = custom_bin / "cargo"
        write_executable(
            rustc,
            """\
            #!/bin/sh
            printf '%s\n' 'rustc 1.0.0 (custom)' \
              'host: x86_64-unknown-linux-gnu'
            """,
        )
        expected_rustc = shlex.quote(str(rustc))
        custom_path_entry = shlex.quote(f":{custom_bin}:")
        write_executable(
            cargo,
            f"""\
            #!/bin/sh
            if [ "${{RUSTC:-}}" != {expected_rustc} ]; then
              printf '%s\n' 'Cargo did not receive the validated absolute rustc' >&2
              exit 84
            fi
            case ":$PATH:" in
              *{custom_path_entry}*)
                printf '%s\n' 'custom tool directory leaked into sanitized PATH' >&2
                exit 83
                ;;
            esac
            "$RUSTC" -vV >/dev/null || exit 82
            printf '%s\n' 'cargo 1.0.0 (custom)'
            """,
        )

        completed = self.run_self_test(
            extra_environment={"RUSTC": "/untrusted/ambient-rustc"},
            path=secure_system_path(custom_bin, self.command_bin),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            f"host=x86_64-unknown-linux-gnu uid={self.uid}\n",
        )
        self.assertNotIn("/untrusted/ambient-rustc", completed.stderr)

    def test_malformed_or_ambiguous_rustc_hosts_fail_before_interpolation(
        self,
    ) -> None:
        self.install_getent()
        malformed_hosts = {
            "duplicate": (
                "x86_64-unknown-linux-gnu\n"
                "host: aarch64-unknown-linux-gnu"
            ),
            "slash": "x86_64-unknown-linux-gnu/evil",
            "path traversal": "x86_64-unknown-linux-gnu/../../tmp",
            "leading whitespace": " x86_64-unknown-linux-gnu",
            "internal whitespace": "x86_64 unknown-linux-gnu",
            "invalid dollar": "x86_64-unknown-linux-gnu$evil",
            "invalid dots": "x86_64-unknown-linux-gnu..evil",
        }
        for label, host in malformed_hosts.items():
            with self.subTest(label=label):
                shutil.rmtree(self.home / ".cargo", ignore_errors=True)
                create_rustup_proxies(self.home, host=host)

                completed = self.run_self_test()

                self.assertEqual(completed.returncode, 77, completed.stderr)
                self.assertIn(
                    "rustc must report exactly one path-safe Linux host target",
                    completed.stderr,
                )
                self.assertEqual(completed.stdout, "")

    def test_malformed_sudo_uid_values_fail_closed_before_account_lookup(self) -> None:
        write_executable(
            self.command_bin / "getent",
            """\
            #!/bin/sh
            printf '%s\n' 'getent must not run for malformed SUDO_UID' >&2
            exit 94
            """,
        )
        for malformed in ("", "abc", "-1", "1/2", "01", "18446744073709551616"):
            with self.subTest(sudo_uid=malformed):
                completed = self.run_self_test(
                    extra_environment={"SUDO_UID": malformed}
                )
                self.assertEqual(completed.returncode, 77, completed.stderr)
                self.assertIn("invoking UID was invalid", completed.stderr)
                self.assertNotIn("getent must not run", completed.stderr)

    def test_missing_passwd_entry_fails_closed(self) -> None:
        self.install_getent(exit_status=2)

        completed = self.run_self_test(
            extra_environment={"SUDO_UID": str(self.uid)}
        )

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("could not resolve the invoking user account", completed.stderr)

    def test_missing_home_fails_closed(self) -> None:
        self.install_getent(home=self.root / "absent-home")

        completed = self.run_self_test(
            extra_environment={"SUDO_UID": str(self.uid)}
        )

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("invoking user home is unavailable or untrusted", completed.stderr)

    def test_missing_home_tool_binaries_fail_closed(self) -> None:
        self.install_getent()

        completed = self.run_self_test(
            extra_environment={"SUDO_UID": str(self.uid)}
        )

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("invoking user Rust toolchain is unavailable", completed.stderr)

    def test_untrusted_rustup_proxy_target_fails_closed(self) -> None:
        self.install_getent()
        cargo_bin = self.home / ".cargo/bin"
        cargo_bin.mkdir(parents=True)
        (cargo_bin / "cargo").symlink_to("/usr/bin/true")
        (cargo_bin / "rustc").symlink_to("/usr/bin/true")

        completed = self.run_self_test()

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("invoking user Rust toolchain is untrusted", completed.stderr)

    def test_group_or_world_writable_rustup_is_rejected(self) -> None:
        self.install_getent()
        for mode in (0o775, 0o757):
            with self.subTest(mode=oct(mode)):
                shutil.rmtree(self.home / ".cargo", ignore_errors=True)
                create_rustup_proxies(self.home)
                (self.home / ".cargo/bin/rustup").chmod(mode)

                completed = self.run_self_test()

                self.assertEqual(completed.returncode, 77, completed.stderr)
                self.assertIn(
                    "invoking user Rust toolchain is untrusted", completed.stderr
                )

    def test_relative_ambient_tool_paths_are_rejected(self) -> None:
        self.install_getent()
        relative_bin = self.root / "relative-bin"
        relative_bin.mkdir()
        for tool in ("cargo", "rustc"):
            write_executable(
                relative_bin / tool,
                """\
                #!/bin/sh
                exit 93
                """,
            )
        relative_path = os.pathsep.join(
            (
                os.path.relpath(relative_bin, REPO_ROOT),
                str(self.command_bin),
                "/usr/sbin",
                "/usr/bin",
                "/sbin",
                "/bin",
            )
        )

        completed = self.run_self_test(path=relative_path)

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("ambient Rust toolchain path is untrusted", completed.stderr)

    def test_forged_sudo_uid_from_non_root_caller_is_rejected(self) -> None:
        mocked_current_uid = 1_234_567
        forged_uid = 1_234_568
        write_executable(
            self.command_bin / "id",
            f"""\
            #!/bin/sh
            [ "$1" = "-u" ] || exit 92
            printf '%s\n' {mocked_current_uid}
            """,
        )
        write_executable(
            self.command_bin / "getent",
            """\
            #!/bin/sh
            printf '%s\n' 'getent must not run for forged SUDO_UID' >&2
            exit 91
            """,
        )

        completed = self.run_self_test(
            extra_environment={"SUDO_UID": str(forged_uid)}
        )

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn(
            "only root may select a different invoking UID", completed.stderr
        )
        self.assertNotIn("getent must not run", completed.stderr)

    def test_mocked_root_without_a_trusted_root_toolchain_fails_cleanly(self) -> None:
        write_executable(
            self.command_bin / "id",
            """\
            #!/bin/sh
            [ "$1" = "-u" ] || exit 92
            printf '%s\n' 0
            """,
        )
        self.install_getent(uid=0, gid=0, home=Path("/root"))

        completed = self.run_self_test()

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertIn("Rust toolchain", completed.stderr)
        self.assertNotIn("/root", completed.stderr)

    def test_failure_diagnostics_do_not_echo_environment_canaries(self) -> None:
        canary = "HOST_RUST_TOOLCHAIN_SECRET_MUST_NOT_LEAK_0123456789"
        self.install_getent(exit_status=2)

        completed = self.run_self_test(
            extra_environment={
                "SUDO_UID": str(self.uid),
                "XENOTEER_TOKEN": canary,
            }
        )

        self.assertEqual(completed.returncode, 77, completed.stderr)
        self.assertNotIn(canary, completed.stdout)
        self.assertNotIn(canary, completed.stderr)


class ToolchainTerritoryContractTests(unittest.TestCase):
    def test_event_flood_does_not_require_ambient_cargo_or_rustc(self) -> None:
        source = EVENT_FLOOD_RUNNER.read_text()
        command_loop = source[source.index("for command in ") : source.index("done", source.index("for command in "))]
        self.assertNotIn("cargo", command_loop)
        self.assertNotIn("rustc", command_loop)
        self.assertLess(source.index("host_rust_toolchain_resolve"), source.index("command -v cargo"))

    def test_phase3_resolves_sudo_identity_before_ambient_cargo_fallback(self) -> None:
        source = PHASE3_RUNNER.read_text()
        for function_name in ("build_recorder", "run_sdk_smoke"):
            function = source[source.index(f"{function_name}()") :]
            function = function[: function.index("\n}")]
            self.assertLess(function.index("SUDO_UID"), function.index("command -v cargo"))
        platform = source[source.index("assert_fixture_platform()") :]
        platform = platform[: platform.index("\n}")]
        self.assertLess(platform.index("SUDO_UID"), platform.index("command -v rustc"))

    def test_viewer_denial_secure_path_has_an_invoking_user_fallback(self) -> None:
        source = VIEWER_DENIAL_RUNNER.read_text()
        self.assertIn("elif [[ -n ${SUDO_UID:-} && $SUDO_UID != 0 ]]", source)
        self.assertLess(source.index("SUDO_UID"), source.index("invoking_cargo="))

    def test_public_quickstarts_uses_validated_build_identity(self) -> None:
        source = PUBLIC_QUICKSTARTS.read_text()
        self.assertIn("class BuildIdentity:", source)
        self.assertIn("trusted_path = self._trusted_path", source)
        self.assertIn("identity = BuildIdentity.current()", source)


class SudoIdentityIntegrationTests(unittest.TestCase):
    def test_real_sudo_secure_path_scrubs_hostile_rust_environment(self) -> None:
        if os.getuid() == 0:
            self.skipTest("real root-to-user integration requires a non-root caller")
        invoking_user = pwd.getpwuid(os.getuid())
        sudo = shutil.which("sudo", path="/usr/sbin:/usr/bin:/sbin:/bin")
        if sudo is None:
            self.skipTest("sudo is unavailable")
        try:
            probe = subprocess.run(
                [sudo, "-n", "true"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=3,
                check=False,
            )
        except subprocess.TimeoutExpired:
            self.skipTest("sudo -n probe timed out")
        if probe.returncode != 0:
            self.skipTest("passwordless sudo is unavailable")
        secure_path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        if shutil.which("cargo", path=secure_path) is not None:
            self.skipTest("secure system PATH already exposes cargo")
        if shutil.which("rustc", path=secure_path) is not None:
            self.skipTest("secure system PATH already exposes rustc")
        cargo = Path(invoking_user.pw_dir) / ".cargo/bin/cargo"
        rustc = Path(invoking_user.pw_dir) / ".cargo/bin/rustc"
        for name, proxy in (("cargo", cargo), ("rustc", rustc)):
            if not proxy.is_file() or not os.access(proxy, os.X_OK):
                self.skipTest(
                    f"invoking user's {name} proxy is absent or non-executable"
                )
        rustc_version = subprocess.run(
            [str(rustc), "-vV"],
            env={
                "HOME": invoking_user.pw_dir,
                "LANG": "C.UTF-8",
                "LC_ALL": "C.UTF-8",
                "PATH": secure_path,
            },
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=True,
        ).stdout
        host_lines = [
            line.removeprefix("host: ")
            for line in rustc_version.splitlines()
            if line.startswith("host: ")
        ]
        self.assertEqual(len(host_lines), 1, rustc_version)
        canary = "REAL_SUDO_RUST_BOUNDARY_SECRET_0123456789"
        completed = subprocess.run(
            [
                sudo,
                "-n",
                "/usr/bin/env",
                f"SUDO_UID={invoking_user.pw_uid}",
                f"SUDO_GID={invoking_user.pw_gid}",
                f"SUDO_USER={invoking_user.pw_name}",
                f"PATH={secure_path}",
                "RUSTC=/untrusted/ambient-rustc",
                "RUSTC_WRAPPER=/untrusted/wrapper",
                "RUSTFLAGS=--cfg hostile",
                "RUSTUP_TOOLCHAIN=hostile-toolchain",
                "CARGO_TARGET_DIR=/untrusted/target",
                f"XENOTEER_TOKEN={canary}",
                f"HOST_RUST_SECRET_CANARY={canary}",
                str(EVENT_FLOOD_RUNNER),
                "--self-test-host-rust-toolchain",
            ],
            cwd=REPO_ROOT,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
            check=False,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            f"host={host_lines[0]} uid={invoking_user.pw_uid}\n",
        )
        self.assertNotIn(canary, completed.stdout)
        self.assertNotIn(canary, completed.stderr)


if __name__ == "__main__":
    unittest.main()
