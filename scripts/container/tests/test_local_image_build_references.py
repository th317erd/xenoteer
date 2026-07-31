#!/usr/bin/env python3
# SPDX-License-Identifier: BUSL-1.1
"""Bounded fake-Docker regressions for local image Dockerfile FROM aliases."""

from __future__ import annotations

import json
import os
import re
import shlex
import signal
import stat
import subprocess
import tempfile
import textwrap
import time
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
HELPER = REPOSITORY_ROOT / "scripts/container/local-image-build-reference.sh"
NOVNC_GATE = REPOSITORY_ROOT / "scripts/container/test-novnc-spike.sh"
BROWSER_GATE = REPOSITORY_ROOT / "scripts/container/test-browser-spike.sh"
FIXTURE_BUILDER = (
    REPOSITORY_ROOT / "scripts/container/build-desktop-app-fixture.sh"
)
FIXTURE_ARTIFACT_LOCK = (
    REPOSITORY_ROOT / "container/fixtures/desktop-apps/artifacts.lock"
)
EXACT_IMAGE_ID = "sha256:" + ("ab" * 32)
FOREIGN_IMAGE_ID = "sha256:" + ("cd" * 32)
DERIVED_IMAGE_ID = "sha256:" + ("ef" * 32)
MANIFEST_IMAGE_ID = "sha256:" + ("12" * 32)
INDEX_IMAGE_ID = "sha256:" + ("34" * 32)
ELECTRON_SHA256 = next(
    line.removeprefix("ELECTRON_LINUX_X64_SHA256=")
    for line in FIXTURE_ARTIFACT_LOCK.read_text(encoding="utf-8").splitlines()
    if line.startswith("ELECTRON_LINUX_X64_SHA256=")
)


def write_executable(path: Path, contents: str) -> None:
    path.write_text(textwrap.dedent(contents), encoding="utf-8")
    path.chmod(0o755)


class FakeDockerCase:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.bin = root / "bin"
        self.bin.mkdir()
        self.log = root / "docker.jsonl"
        self.state = root / "docker-state.json"
        self.state.write_text(
            '{"aliases": {}, "alias_inspects": {}, "images": {}, '
            '"containers": {}, "build_outputs": [], '
            '"source_inspects": 0, "source_metadata_inspects": 0, '
            '"source_present": true}',
            encoding="utf-8",
        )
        write_executable(
            self.bin / "docker",
            """\
            #!/usr/bin/python3
            import fcntl
            import json
            import os
            import signal
            import sys
            import time

            args = sys.argv[1:]
            with open(os.environ["FAKE_DOCKER_LOG"], "a", encoding="utf-8") as log:
                fcntl.flock(log, fcntl.LOCK_EX)
                log.write(json.dumps(args) + "\\n")
            state_path = os.environ["FAKE_DOCKER_STATE"]
            state_file = open(state_path, "r+", encoding="utf-8")
            fcntl.flock(state_file, fcntl.LOCK_EX)
            state = json.load(state_file)
            exact = os.environ["FAKE_EXACT_IMAGE_ID"]
            source = os.environ.get("FAKE_SOURCE_REFERENCE", exact)
            foreign = os.environ.get("FAKE_FOREIGN_IMAGE_ID", exact)
            derived = os.environ["FAKE_DERIVED_IMAGE_ID"]
            manifest = os.environ["FAKE_MANIFEST_IMAGE_ID"]
            image_index = os.environ["FAKE_INDEX_IMAGE_ID"]
            iid_mode = os.environ.get("FAKE_IID_MODE", "classic")

            def finish(status=0, output=None):
                state_file.seek(0)
                json.dump(state, state_file, sort_keys=True)
                state_file.truncate()
                state_file.close()
                if output is not None:
                    print(output)
                raise SystemExit(status)

            def block_forever():
                with open(
                    os.environ["FAKE_BLOCK_PID_FILE"],
                    "w",
                    encoding="ascii",
                ) as pid_file:
                    pid_file.write(str(os.getpid()))
                    pid_file.flush()
                    os.fsync(pid_file.fileno())
                while True:
                    time.sleep(1)

            block_on = os.environ.get("FAKE_BLOCK_ON")
            if block_on == "build" and args and args[0] == "build":
                block_forever()

            if args[:2] == ["image", "inspect"]:
                references = []
                format_value = ""
                index = 2
                while index < len(args):
                    if args[index] == "--":
                        index += 1
                        continue
                    if args[index] == "--format":
                        format_value = args[index + 1]
                        break
                    references.append(args[index])
                    index += 1
                reference = references[0]
                if (
                    len(references) == 1
                    and reference in (source, exact)
                    and (
                        not format_value
                        or (
                            "RepoTags" in format_value
                            and "RepoDigests" in format_value
                        )
                    )
                ):
                    state["source_metadata_inspects"] += 1
                    metadata_inspects = state["source_metadata_inspects"]
                    if (
                        os.environ.get("FAKE_SOURCE_METADATA_FAIL") == "1"
                        or (
                            os.environ.get("FAKE_SOURCE_METADATA_FAIL_AFTER") == "1"
                            and metadata_inspects > 1
                        )
                    ):
                        finish(75)
                    if os.environ.get("FAKE_SOURCE_METADATA_MALFORMED") == "1":
                        finish(output='{"not": "an image inspect object"}')
                    metadata_mode = os.environ.get(
                        "FAKE_SOURCE_METADATA_MODE",
                        "tagged",
                    )
                    if (
                        os.environ.get(
                            "FAKE_SOURCE_METADATA_DANGLING_AFTER"
                        )
                        == "1"
                        and metadata_inspects > 1
                    ):
                        metadata_mode = "dangling"
                    repo_tags = (
                        ["<none>:<none>"]
                        if metadata_mode == "dangling"
                        else (
                            []
                            if metadata_mode == "digest"
                            else ["xenoteer:durable-source"]
                        )
                    )
                    repo_digests = (
                        [f"xenoteer@{exact}"]
                        if metadata_mode == "digest"
                        else (
                            ["<none>@<none>"]
                            if metadata_mode == "dangling"
                            else []
                        )
                    )
                    metadata_id = (
                        "SHA256:not-lowercase"
                        if os.environ.get("FAKE_MALFORMED_SOURCE") == "1"
                        and reference == source
                        else exact
                    )
                    image_metadata = {
                        "Id": metadata_id,
                        "RepoTags": repo_tags,
                        "RepoDigests": repo_digests,
                    }
                    finish(
                        output=json.dumps(
                            image_metadata
                            if format_value
                            else [image_metadata]
                        )
                    )
                if ".Id" in format_value:
                    if reference in (source, exact):
                        if not state["source_present"]:
                            finish(1)
                        state["source_inspects"] += 1
                        if os.environ.get("FAKE_MALFORMED_SOURCE") == "1" and reference == source:
                            finish(output="SHA256:not-lowercase")
                        disappear_after = int(os.environ.get("FAKE_SOURCE_DISAPPEAR_AFTER", "0"))
                        if disappear_after and state["source_inspects"] > disappear_after:
                            finish(1)
                        finish(output=exact)
                    if reference in state["aliases"]:
                        count = state["alias_inspects"].get(reference, 0) + 1
                        state["alias_inspects"][reference] = count
                        drift_at = int(os.environ.get("FAKE_ALIAS_DRIFT_AT", "0"))
                        if drift_at and count >= drift_at:
                            finish(output=foreign)
                        finish(output=state["aliases"][reference])
                    if reference in state["images"]:
                        finish(output=state["images"][reference])
                    if reference == derived:
                        if iid_mode == "containerd":
                            finish(1)
                        finish(output=derived)
                    if reference == manifest:
                        finish(output=manifest)
                    if (
                        os.environ.get("FAKE_COLLISION") == "1"
                        and reference.startswith("xenoteer-local-build/")
                    ):
                        finish(output=foreign)
                    finish(1)
                if "distributable" in format_value:
                    finish(output="false")
                if "ExposedPorts" in format_value:
                    finish(output="{}")
                if "fixture.base-image-id" in format_value:
                    finish(output=exact)
                if "fixture.electron-linux-x64-sha256" in format_value:
                    finish(output=os.environ["FAKE_ELECTRON_SHA256"])
                if (
                    len(references) == 2
                    and references[0] == exact
                ):
                    output_reference = references[1]
                    if output_reference in state["images"]:
                        output_id = state["images"][output_reference]
                    elif output_reference == derived and iid_mode == "classic":
                        output_id = derived
                    elif output_reference == exact:
                        output_id = exact
                    else:
                        finish(1)
                    if (
                        os.environ.get("FAKE_RETAG_OUTPUT_BEFORE_PROOF")
                        == "1"
                    ):
                        state["images"][output_reference] = foreign
                        output_id = foreign
                    bad_prefix = os.environ.get("FAKE_BAD_LAYER_PREFIX") == "1"
                    inspect_status = (
                        75
                        if os.environ.get("FAKE_DERIVATION_INSPECT_FAIL") == "1"
                        else 0
                    )
                    if os.environ.get("FAKE_RETAG_OUTPUT_AFTER_PROOF") == "1":
                        for image_reference, image_id in tuple(
                            state["images"].items()
                        ):
                            if image_id == output_id:
                                state["images"][image_reference] = foreign
                    output_metadata = {
                        "Id": output_id,
                        "RepoTags": [output_reference],
                        "RootFS": {
                            "Layers": (
                                ["foreign-a", "derived-c"]
                                if bad_prefix
                                else ["base-a", "base-b", "derived-c"]
                            )
                        },
                    }
                    repo_tags_mode = os.environ.get(
                        "FAKE_OUTPUT_REPOTAGS_MODE",
                        "exact",
                    )
                    if repo_tags_mode == "familiar":
                        output_metadata["RepoTags"] = [
                            output_reference.removeprefix(
                                "docker.io/library/"
                            )
                        ]
                    elif repo_tags_mode == "implicit-latest":
                        output_metadata["RepoTags"] = [
                            f"{output_reference}:latest"
                        ]
                    elif repo_tags_mode == "null":
                        output_metadata["RepoTags"] = None
                    elif repo_tags_mode == "empty":
                        output_metadata["RepoTags"] = []
                    elif repo_tags_mode == "non-string":
                        output_metadata["RepoTags"] = [
                            output_reference,
                            42,
                        ]
                    elif repo_tags_mode == "dangling":
                        output_metadata["RepoTags"] = ["<none>:<none>"]
                    if (
                        (
                            iid_mode == "containerd"
                            or os.environ.get("FAKE_CLASSIC_DESCRIPTOR") == "1"
                            or output_id == image_index
                        )
                        and os.environ.get("FAKE_DESCRIPTOR_OMIT") != "1"
                    ):
                        descriptor_digest = (
                            foreign
                            if os.environ.get(
                                "FAKE_DESCRIPTOR_DIGEST_MISMATCH"
                            )
                            == "1"
                            else output_id
                            if iid_mode == "containerd"
                            else manifest
                        )
                        config_digest = os.environ.get(
                            "FAKE_DESCRIPTOR_CONFIG_DIGEST",
                            (
                                foreign
                                if os.environ.get(
                                    "FAKE_RETAG_OUTPUT_BEFORE_PROOF"
                                )
                                == "1"
                                else derived
                            ),
                        )
                        descriptor_size = (
                            "oversized"
                            if os.environ.get(
                                "FAKE_DESCRIPTOR_SIZE_INVALID"
                            )
                            == "1"
                            else 1_234
                        )
                        descriptor_annotations = (
                            {}
                            if output_id == image_index
                            else
                            ["not", "an", "annotation map"]
                            if os.environ.get(
                                "FAKE_DESCRIPTOR_ANNOTATIONS_INVALID"
                            )
                            == "1"
                            else {
                                "config.digest": (
                                    123
                                    if os.environ.get(
                                        "FAKE_DESCRIPTOR_CONFIG_TYPE_INVALID"
                                    )
                                    == "1"
                                    else config_digest
                                ),
                            }
                        )
                        output_metadata["Descriptor"] = {
                            "mediaType": (
                                "application/vnd.oci.image.index.v1+json"
                                if output_id == image_index
                                else os.environ.get(
                                    "FAKE_DESCRIPTOR_MEDIA_TYPE",
                                    "application/vnd.oci.image.manifest.v1+json",
                                )
                            ),
                            "digest": descriptor_digest,
                            "size": descriptor_size,
                            "annotations": descriptor_annotations,
                        }
                    metadata_values = [
                        {
                            "Id": exact,
                            "RootFS": {
                                "Layers": ["base-a", "base-b"],
                            },
                        },
                        output_metadata,
                    ]
                    if (
                        os.environ.get("FAKE_DERIVATION_METADATA_EXTRA")
                        == "1"
                    ):
                        metadata_values.append(output_metadata)
                    encoded_metadata = json.dumps(metadata_values)
                    if (
                        os.environ.get(
                            "FAKE_DERIVATION_METADATA_OVERSIZE"
                        )
                        == "1"
                    ):
                        encoded_metadata += "x" * 1_048_577
                    finish(
                        inspect_status,
                        output=encoded_metadata,
                    )
                finish(1)
            if args[:2] == ["image", "ls"]:
                if os.environ.get("FAKE_ALIAS_ABSENCE_PROBE_FAIL") == "1":
                    finish(75)
                if os.environ.get("FAKE_COLLISION") == "1":
                    finish(output=foreign)
                finish(output="")
            if args[:2] == ["image", "tag"]:
                if os.environ.get("FAKE_TAG_FAIL") == "1":
                    finish(73)
                state["aliases"][args[3]] = exact
                if os.environ.get("FAKE_SIGNAL_DURING_TAG") == "1":
                    state_file.seek(0)
                    json.dump(state, state_file, sort_keys=True)
                    state_file.truncate()
                    state_file.flush()
                    os.kill(os.getppid(), signal.SIGTERM)
                    time.sleep(0.1)
                finish()
            if args[:2] == ["image", "rm"]:
                if os.environ.get("FAKE_RM_FAIL") == "1":
                    finish(74)
                state["aliases"].pop(args[2], None)
                if (
                    os.environ.get("FAKE_DELETE_LAST_REFERENCE") == "1"
                    and os.environ.get("FAKE_SOURCE_METADATA_MODE") == "dangling"
                    and not state["aliases"]
                ):
                    state["source_present"] = False
                finish()
            if args and args[0] == "build":
                scalar_options = {}
                array_options = {
                    "attest": [],
                    "build-arg": [],
                    "label": [],
                    "output": [],
                    "platform": [],
                    "tag": [],
                }
                boolean_options = set()
                long_value_options = {
                    "--attest": ("attest", True),
                    "--build-arg": ("build-arg", True),
                    "--builder": ("builder", False),
                    "--cpu-period": ("cpu-period", False),
                    "--cpu-quota": ("cpu-quota", False),
                    "--file": ("file", False),
                    "--iidfile": ("iidfile", False),
                    "--label": ("label", True),
                    "--memory": ("memory", False),
                    "--output": ("output", True),
                    "--platform": ("platform", True),
                    "--provenance": ("provenance", False),
                    "--sbom": ("sbom", False),
                    "--tag": ("tag", True),
                }
                short_value_options = {
                    "-f": ("file", False),
                    "-o": ("output", True),
                    "-t": ("tag", True),
                }
                long_boolean_options = {
                    "--load": "load",
                    "--no-cache": "no-cache",
                    "--push": "push",
                }
                contexts = []
                parse_options = True
                option_index = 1
                while option_index < len(args):
                    argument = args[option_index]
                    if parse_options and argument == "--":
                        parse_options = False
                        option_index += 1
                        continue
                    if not parse_options or not argument.startswith("-"):
                        contexts.append(argument)
                        option_index += 1
                        continue
                    if argument in long_boolean_options:
                        boolean_options.add(long_boolean_options[argument])
                        option_index += 1
                        continue
                    if argument.startswith("--"):
                        option_name, separator, option_value = argument.partition("=")
                        option_contract = long_value_options.get(option_name)
                        if option_contract is None:
                            finish(98)
                        if not separator:
                            if (
                                option_index + 1 >= len(args)
                                or args[option_index + 1].startswith("-")
                            ):
                                finish(98)
                            option_value = args[option_index + 1]
                            option_index += 1
                        if not option_value:
                            finish(98)
                    else:
                        option_name = argument[:2]
                        option_contract = short_value_options.get(option_name)
                        if option_contract is None:
                            finish(98)
                        if len(argument) == 2:
                            if (
                                option_index + 1 >= len(args)
                                or args[option_index + 1].startswith("-")
                            ):
                                finish(98)
                            option_value = args[option_index + 1]
                            option_index += 1
                        else:
                            option_value = argument[2:]
                            if option_value.startswith("="):
                                option_value = option_value[1:]
                        if not option_value:
                            finish(98)
                    option_key, repeatable = option_contract
                    if repeatable:
                        array_options[option_key].append(option_value)
                    else:
                        scalar_options[option_key] = option_value
                    option_index += 1
                if len(contexts) != 1:
                    finish(98)
                state["build_outputs"] = array_options["output"]

                for argument in array_options["build-arg"]:
                    if (
                        argument.startswith("SPIKE_BASE_IMAGE=sha256:")
                        or argument.startswith("XENOTEER_RUNTIME_IMAGE=sha256:")
                        or argument.startswith("XENOTEER_BASE_IMAGE=sha256:")
                    ):
                        finish(86)
                    if (
                        argument.startswith("SPIKE_BASE_IMAGE=xenoteer-local-build/")
                        or argument.startswith("XENOTEER_RUNTIME_IMAGE=xenoteer-local-build/")
                        or argument.startswith("XENOTEER_BASE_IMAGE=xenoteer-local-build/")
                    ):
                        alias = argument.split("=", 1)[1]
                        if state["aliases"].get(alias) != exact:
                            finish(87)

                provenance_disabled = (
                    scalar_options.get("provenance") == "false"
                )
                sbom_disabled = scalar_options.get("sbom") == "false"
                single_platform = (
                    len(array_options["platform"]) <= 1
                    and all(
                        value and "," not in value
                        for value in array_options["platform"]
                    )
                )
                emits_index = (
                    os.environ.get("FAKE_DEFAULT_ATTESTATION_INDEX") == "1"
                    and (
                        not provenance_disabled
                        or not sbom_disabled
                        or bool(array_options["attest"])
                    )
                ) or not single_platform
                build_failure = int(os.environ.get("FAKE_BUILD_FAIL", "0"))
                if array_options["tag"]:
                    derived_id = (
                        exact
                        if os.environ.get("FAKE_DERIVED_IS_BASE") == "1"
                        else derived
                    )
                    output_id = (
                        image_index
                        if emits_index and derived_id != exact
                        else
                        manifest
                        if iid_mode == "containerd"
                        and derived_id != exact
                        else derived_id
                    )
                    non_image_output = any(
                        output == "local"
                        or output.startswith("type=local,")
                        or output == "oci"
                        or output.startswith("type=oci,")
                        or output == "tar"
                        or output.startswith("type=tar,")
                        for output in array_options["output"]
                    )
                    if "push" not in boolean_options and not non_image_output:
                        for tag in array_options["tag"]:
                            state["images"][tag] = output_id
                    iid_path = scalar_options.get("iidfile")
                    if iid_path is not None:
                        if os.environ.get("FAKE_REQUIRE_ABSENT_IID") == "1":
                            reservation_stat = os.lstat(
                                os.path.dirname(iid_path)
                            )
                            if (
                                os.path.lexists(iid_path)
                                or not os.path.isdir(os.path.dirname(iid_path))
                                or reservation_stat.st_uid != os.geteuid()
                                or reservation_stat.st_mode & 0o7777 != 0o700
                            ):
                                finish(89)
                        if (
                            os.environ.get("FAKE_RECREATE_IID") == "1"
                            and os.path.lexists(iid_path)
                        ):
                            os.unlink(iid_path)
                        with open(
                            iid_path,
                            "w",
                            encoding="ascii",
                        ) as iid_file:
                            iid_file.write(derived_id + "\\n")
                        os.chmod(iid_path, 0o644)
                        iid_mutation = os.environ.get("FAKE_IID_MUTATION")
                        if iid_mutation == "fifo":
                            os.unlink(iid_path)
                            os.mkfifo(iid_path, 0o600)
                        elif iid_mutation == "symlink":
                            os.unlink(iid_path)
                            os.symlink(state_path, iid_path)
                        elif iid_mutation == "hardlink":
                            os.link(
                                iid_path,
                                state_path + ".iid-hardlink",
                            )
                        elif iid_mutation == "oversize":
                            with open(iid_path, "ab") as iid_file:
                                iid_file.write(b"x")
                        elif iid_mutation == "mode":
                            os.chmod(iid_path, 0o666)
                finish(build_failure)
            if args and args[0] == "inspect":
                if "State.Running" in " ".join(args):
                    finish(output="true")
                finish()
            if args and args[0] == "run":
                if "--name" in args:
                    state["containers"][args[args.index("--name") + 1]] = True
                if block_on == "run":
                    block_forever()
                finish()
            if args and args[0] == "rm":
                state["containers"].pop(args[-1], None)
                finish()
            if args and args[0] in {
                "exec",
                "logs",
                "stop",
            }:
                finish()
            finish(97)
            """,
        )
        write_executable(
            self.bin / "id",
            """\
            #!/bin/sh
            if [ "${1:-}" = -u ]; then
                printf '%s\\n' 0
            else
                exec /usr/bin/id "$@"
            fi
            """,
        )
        write_executable(self.bin / "chown", "#!/bin/sh\nexit 0\n")

    def environment(self, **overrides: str) -> dict[str, str]:
        environment = {
            "FAKE_DOCKER_LOG": str(self.log),
            "FAKE_DOCKER_STATE": str(self.state),
            "FAKE_EXACT_IMAGE_ID": EXACT_IMAGE_ID,
            "FAKE_FOREIGN_IMAGE_ID": FOREIGN_IMAGE_ID,
            "FAKE_DERIVED_IMAGE_ID": DERIVED_IMAGE_ID,
            "FAKE_MANIFEST_IMAGE_ID": MANIFEST_IMAGE_ID,
            "FAKE_INDEX_IMAGE_ID": INDEX_IMAGE_ID,
            "FAKE_ELECTRON_SHA256": ELECTRON_SHA256,
            "HOME": str(self.root),
            "LANG": "C",
            "LC_ALL": "C",
            "PATH": os.pathsep.join(
                (
                    str(self.bin),
                    "/usr/sbin",
                    "/usr/bin",
                    "/sbin",
                    "/bin",
                )
            ),
        }
        environment.update(overrides)
        return environment

    def calls(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [
            json.loads(line)
            for line in self.log.read_text(encoding="utf-8").splitlines()
        ]


class LocalImageBuildReferenceTests(unittest.TestCase):
    def run_command(
        self,
        command: list[str],
        environment: dict[str, str],
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            command,
            cwd=REPOSITORY_ROOT,
            env=environment,
            capture_output=True,
            check=False,
            text=True,
            timeout=8,
        )

    def run_command_in_killable_group(
        self,
        command: list[str],
        environment: dict[str, str],
        *,
        timeout_seconds: float = 3,
    ) -> subprocess.CompletedProcess[str]:
        process = subprocess.Popen(
            command,
            cwd=REPOSITORY_ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=timeout_seconds)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=3)
            return subprocess.CompletedProcess(
                command,
                124,
                stdout,
                stderr + "\ncommand exceeded its bounded test timeout",
            )
        return subprocess.CompletedProcess(
            command,
            process.returncode,
            stdout,
            stderr,
        )

    def helper_driver(
        self,
        root: Path,
        body: str,
    ) -> Path:
        driver = root / "driver.sh"
        write_executable(
            driver,
            f"""\
            #!/bin/sh
            set -eu
            . {shlex.quote(str(HELPER))}
            cleanup() {{
                original_status=$?
                trap - EXIT HUP INT TERM
                set +e
                xenoteer_cleanup_local_image_alias
                cleanup_status=$?
                if [ "$original_status" -ne 0 ]; then
                    exit "$original_status"
                fi
                exit "$cleanup_status"
            }}
            trap cleanup EXIT
            trap 'exit 129' HUP
            trap 'exit 130' INT
            trap 'exit 143' TERM
            {body}
            """,
        )
        return driver

    def assert_fake_reservations_removed(self, fake: FakeDockerCase) -> None:
        aliases = [
            call[3]
            for call in fake.calls()
            if call[:2] == ["image", "tag"]
        ]
        self.assertTrue(aliases)
        for alias in aliases:
            nonce = alias.rsplit(":", 1)[1]
            self.assertFalse(
                Path(f"/tmp/xenoteer-local-image-{nonce}").exists(),
                alias,
            )

    def securely_remove_exact_test_reservation(
        self,
        reservation: Path,
        *,
        require_residue: bool,
    ) -> None:
        if re.fullmatch(
            r"/tmp/xenoteer-local-image-[0-9a-f]{32}",
            str(reservation),
        ) is None:
            raise AssertionError(
                f"refusing to clean an unexpected reservation: {reservation}"
            )
        if not os.path.lexists(reservation):
            if require_residue:
                raise AssertionError(
                    f"expected reservation residue is absent: {reservation}"
                )
            return

        directory_flags = os.O_RDONLY
        directory_flags |= getattr(os, "O_CLOEXEC", 0)
        directory_flags |= getattr(os, "O_DIRECTORY", 0)
        directory_flags |= getattr(os, "O_NOFOLLOW", 0)
        directory = os.open(reservation, directory_flags)
        try:
            directory_stat = os.fstat(directory)
            directory_identity = (
                directory_stat.st_dev,
                directory_stat.st_ino,
                directory_stat.st_uid,
                directory_stat.st_nlink,
            )
            if (
                not stat.S_ISDIR(directory_stat.st_mode)
                or directory_stat.st_uid != os.geteuid()
                or stat.S_IMODE(directory_stat.st_mode) != 0o700
            ):
                raise AssertionError(
                    f"reservation metadata is unsafe: {reservation}"
                )

            entries = os.listdir(directory)
            if entries and entries != ["derived-image-id"]:
                raise AssertionError(
                    f"reservation has unexpected children: {entries!r}"
                )
            if not entries:
                if require_residue:
                    raise AssertionError(
                        f"expected IID residue is absent: {reservation}"
                    )
            else:
                iid_flags = os.O_RDONLY | os.O_NONBLOCK
                iid_flags |= getattr(os, "O_CLOEXEC", 0)
                iid_flags |= getattr(os, "O_NOFOLLOW", 0)
                iid = os.open(
                    "derived-image-id",
                    iid_flags,
                    dir_fd=directory,
                )
                try:
                    iid_stat = os.fstat(iid)
                    iid_identity = (
                        iid_stat.st_dev,
                        iid_stat.st_ino,
                        iid_stat.st_uid,
                    )
                    expected_contents = (DERIVED_IMAGE_ID + "\n").encode(
                        "ascii"
                    )
                    if (
                        not stat.S_ISREG(iid_stat.st_mode)
                        or iid_stat.st_uid != os.geteuid()
                        or iid_stat.st_nlink != 1
                        or stat.S_IMODE(iid_stat.st_mode) != 0o644
                        or iid_stat.st_size != len(expected_contents)
                        or os.read(iid, len(expected_contents) + 1)
                        != expected_contents
                        or os.read(iid, 1) != b""
                    ):
                        raise AssertionError(
                            f"IID residue metadata is unsafe: {reservation}"
                        )
                    path_stat = os.stat(
                        "derived-image-id",
                        dir_fd=directory,
                        follow_symlinks=False,
                    )
                    if (
                        path_stat.st_dev,
                        path_stat.st_ino,
                        path_stat.st_uid,
                    ) != iid_identity:
                        raise AssertionError(
                            f"IID residue identity changed: {reservation}"
                        )
                finally:
                    os.close(iid)
                os.unlink("derived-image-id", dir_fd=directory)

            final_stat = os.fstat(directory)
            if (
                (
                    final_stat.st_dev,
                    final_stat.st_ino,
                    final_stat.st_uid,
                    final_stat.st_nlink,
                )
                != directory_identity
                or os.listdir(directory)
            ):
                raise AssertionError(
                    f"reservation changed during cleanup: {reservation}"
                )
        finally:
            os.close(directory)

        os.rmdir(reservation)
        if os.path.lexists(reservation):
            raise AssertionError(
                f"reservation survived exact cleanup: {reservation}"
            )

    def assert_signal_stops_guarded_docker(
        self,
        *,
        block_on: str,
        command: list[str] | None = None,
        driver_body: str | None = None,
        signal_number: signal.Signals,
        expected_status: int,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix=f"xenoteer-local-{block_on}-signal-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            child_pid_file = root / "blocked-docker.pid"
            if driver_body is not None:
                self.assertIsNone(command)
                command = [str(self.helper_driver(root, driver_body))]
            process = subprocess.Popen(
                command or [str(NOVNC_GATE)],
                cwd=REPOSITORY_ROOT,
                env=fake.environment(
                    FAKE_BLOCK_ON=block_on,
                    FAKE_BLOCK_PID_FILE=str(child_pid_file),
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            deadline = time.monotonic() + 3
            while not child_pid_file.exists():
                if process.poll() is not None:
                    stdout, stderr = process.communicate()
                    self.fail(
                        f"gate exited before blocking {block_on}: "
                        f"{process.returncode}\\n{stdout}\\n{stderr}"
                    )
                if time.monotonic() >= deadline:
                    process.kill()
                    process.communicate()
                    self.fail(f"fake Docker never blocked during {block_on}")
                time.sleep(0.01)

            child_pid = int(child_pid_file.read_text(encoding="ascii"))
            os.kill(process.pid, signal_number)
            try:
                stdout, stderr = process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.kill()
                stdout, stderr = process.communicate(timeout=3)
                self.fail(
                    f"{signal_number.name} did not stop/reap foreground "
                    f"Docker {block_on} within three seconds\\n{stdout}\\n{stderr}"
                )

            self.assertEqual(process.returncode, expected_status, stderr)
            child_deadline = time.monotonic() + 1
            while True:
                try:
                    os.kill(child_pid, 0)
                except ProcessLookupError:
                    break
                if time.monotonic() >= child_deadline:
                    self.fail(
                        f"foreground Docker {block_on} child {child_pid} "
                        "survived signal cleanup"
                    )
                time.sleep(0.01)
            calls = fake.calls()
            self.assertTrue(
                any(call[:2] == ["image", "rm"] for call in calls),
                calls,
            )
            state = json.loads(fake.state.read_text(encoding="utf-8"))
            self.assertEqual(state["aliases"], {})
            self.assertEqual(state["containers"], {})
            if block_on == "run":
                self.assertTrue(
                    any(call[0] == "rm" for call in calls),
                    calls,
                )

    def test_novnc_raw_local_id_is_aliased_before_docker_build(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-novnc-local-id-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            build = next(call for call in calls if call[0] == "build")
            self.assertNotIn(f"SPIKE_BASE_IMAGE={EXACT_IMAGE_ID}", build)
            self.assertTrue(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertTrue(any(call[:2] == ["image", "rm"] for call in calls))
            self.assertTrue(
                any(
                    call[:4]
                    == [
                        "image",
                        "inspect",
                        EXACT_IMAGE_ID,
                        "xenoteer:novnc-spike",
                    ]
                    for call in calls
                )
            )

    def test_containerd_config_iid_maps_to_exact_tagged_manifest(self) -> None:
        output_tag = "xenoteer:containerd-iid-test"
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-containerd-iid-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_DESCRIPTOR_MEDIA_TYPE=(
                        "application/vnd.docker.distribution.manifest.v2+json"
                    ),
                    FAKE_IID_MODE="containerd",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                    XENOTEER_NOVNC_SPIKE_IMAGE=output_tag,
                ),
            )

            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertTrue(
                any(
                    call[:4]
                    == ["image", "inspect", EXACT_IMAGE_ID, output_tag]
                    for call in calls
                ),
                calls,
            )
            self.assertFalse(
                any(
                    call[:3] == ["image", "inspect", DERIVED_IMAGE_ID]
                    for call in calls
                ),
                calls,
            )
            runs = [call for call in calls if call[0] == "run"]
            self.assertEqual(len(runs), 1)
            self.assertEqual(runs[0][-1], MANIFEST_IMAGE_ID)

    def test_classic_direct_iid_accepts_a_consistent_descriptor(self) -> None:
        output_tag = "xenoteer:classic-descriptor-test"
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-classic-descriptor-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_CLASSIC_DESCRIPTOR="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                    XENOTEER_NOVNC_SPIKE_IMAGE=output_tag,
                ),
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            runs = [call for call in fake.calls() if call[0] == "run"]
            self.assertEqual(len(runs), 1)
            self.assertEqual(runs[0][-1], DERIVED_IMAGE_ID)

    def test_all_local_derived_builds_disable_default_attestations(
        self,
    ) -> None:
        cases = (
            (
                "novnc",
                [str(NOVNC_GATE)],
                {
                    "XENOTEER_NOVNC_SPIKE_BASE_IMAGE": EXACT_IMAGE_ID,
                    "XENOTEER_NOVNC_SPIKE_IMAGE": "xenoteer:novnc-attestation-test",
                },
            ),
            (
                "browser",
                [
                    str(BROWSER_GATE),
                    EXACT_IMAGE_ID,
                    "xenoteer:browser-attestation-test",
                ],
                {},
            ),
            (
                "fixture",
                [str(FIXTURE_BUILDER)],
                {
                    "XENOTEER_IMAGE": EXACT_IMAGE_ID,
                    "XENOTEER_DESKTOP_APPS_IMAGE": (
                        "xenoteer:fixture-attestation-test"
                    ),
                },
            ),
        )
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-build-attestation-test-",
        ) as temporary:
            root = Path(temporary)
            for index, (name, command, overrides) in enumerate(cases):
                with self.subTest(name=name):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        command,
                        fake.environment(
                            **overrides,
                            FAKE_DEFAULT_ATTESTATION_INDEX="1",
                            FAKE_IID_MODE="containerd",
                        ),
                    )

                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    build = next(
                        call for call in fake.calls() if call[0] == "build"
                    )
                    self.assertIn("--provenance=false", build)
                    self.assertIn("--sbom=false", build)
                    self.assertFalse(
                        any(
                            argument == "--attest"
                            or argument.startswith("--attest=")
                            for argument in build
                        ),
                        build,
                    )

    def test_fixture_rejects_ambiguous_manifest_build_options_before_docker(
        self,
    ) -> None:
        cases = (
            ("provenance-equals", ["--provenance=true"], "attestation"),
            ("provenance-split", ["--provenance", "false"], "attestation"),
            ("sbom-equals", ["--sbom=true"], "attestation"),
            ("sbom-split", ["--sbom", "false"], "attestation"),
            (
                "attest-equals",
                ["--attest=type=provenance,disabled=true"],
                "attestation",
            ),
            (
                "attest-split",
                ["--attest", "type=sbom,disabled=false"],
                "attestation",
            ),
            (
                "multi-platform-equals",
                ["--platform=linux/amd64,linux/arm64"],
                "single-platform",
            ),
            (
                "multi-platform-split",
                ["--platform", "linux/amd64,linux/arm64"],
                "single-platform",
            ),
            (
                "repeated-platform",
                [
                    "--platform=linux/amd64",
                    "--platform",
                    "linux/arm64",
                ],
                "single-platform",
            ),
            ("empty-platform", ["--platform="], "single-platform"),
            ("missing-platform", ["--platform"], "single-platform"),
            ("bare-option-terminator", ["--"], "not permitted"),
            ("positional-context", ["."], "not permitted"),
            ("short-tag-split", ["-t", "xenoteer:foreign"], "not permitted"),
            ("short-tag-joined", ["-txenoteer:foreign"], "not permitted"),
            ("long-tag", ["--tag=xenoteer:foreign"], "not permitted"),
            ("short-file", ["-f", "Dockerfile.foreign"], "not permitted"),
            ("long-file", ["--file=Dockerfile.foreign"], "not permitted"),
            (
                "owned-iidfile",
                ["--iidfile=/tmp/foreign-iid"],
                "not permitted",
            ),
            (
                "short-output",
                ["-o", "type=local,dest=/tmp/foreign-output"],
                "not permitted",
            ),
            (
                "long-output",
                ["--output=type=local,dest=/tmp/foreign-output"],
                "not permitted",
            ),
            ("push", ["--push"], "not permitted"),
            ("load", ["--load"], "not permitted"),
            ("other-control", ["--target", "foreign-stage"], "not permitted"),
            ("unknown-control", ["--definitely-unknown"], "not permitted"),
        )
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-build-options-test-",
        ) as temporary:
            root = Path(temporary)
            for index, (name, arguments, expected_error) in enumerate(cases):
                with self.subTest(name=name):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        [str(FIXTURE_BUILDER), *arguments],
                        fake.environment(XENOTEER_IMAGE=EXACT_IMAGE_ID),
                    )

                    self.assertNotEqual(completed.returncode, 0)
                    self.assertIn(expected_error, completed.stderr)
                    self.assertEqual(fake.calls(), [])

    def test_fixture_rejects_invalid_allowlisted_build_options_before_docker(
        self,
    ) -> None:
        cases = (
            ("builder-missing", ["--builder"]),
            ("builder-empty", ["--builder="]),
            ("builder-option-shaped", ["--builder", "--no-cache"]),
            ("builder-invalid", ["--builder", "unsafe/name"]),
            ("builder-too-long", ["--builder=" + ("a" * 129)]),
            ("builder-duplicate", ["--builder=a", "--builder", "b"]),
            ("platform-invalid", ["--platform=linux"]),
            (
                "platform-component-too-long",
                ["--platform=linux/" + ("a" * 33)],
            ),
            ("cpu-period-missing", ["--cpu-period"]),
            ("cpu-period-small", ["--cpu-period=999"]),
            ("cpu-period-large", ["--cpu-period=1000001"]),
            ("cpu-period-nonnumeric", ["--cpu-period", "fast"]),
            (
                "cpu-period-duplicate",
                ["--cpu-period=100000", "--cpu-period", "200000"],
            ),
            ("cpu-quota-missing", ["--cpu-quota"]),
            ("cpu-quota-zero", ["--cpu-quota=0"]),
            ("cpu-quota-large", ["--cpu-quota=1000000001"]),
            ("cpu-quota-negative", ["--cpu-quota", "-1"]),
            (
                "cpu-quota-duplicate",
                ["--cpu-quota=100000", "--cpu-quota", "200000"],
            ),
            ("memory-missing", ["--memory"]),
            ("memory-empty", ["--memory="]),
            ("memory-option-shaped", ["--memory", "--no-cache"]),
            ("memory-zero", ["--memory=0"]),
            ("memory-invalid", ["--memory", "lots"]),
            ("memory-large", ["--memory=1025g"]),
            ("memory-duplicate", ["--memory=4g", "--memory", "6g"]),
            ("no-cache-value", ["--no-cache=true"]),
            ("no-cache-duplicate", ["--no-cache", "--no-cache"]),
        )
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-invalid-allowlist-test-",
        ) as temporary:
            root = Path(temporary)
            for index, (name, arguments) in enumerate(cases):
                with self.subTest(name=name):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        [str(FIXTURE_BUILDER), *arguments],
                        fake.environment(XENOTEER_IMAGE=EXACT_IMAGE_ID),
                    )

                    self.assertNotEqual(completed.returncode, 0)
                    self.assertIn(
                        "invalid fixture build option",
                        completed.stderr,
                    )
                    self.assertEqual(fake.calls(), [])

    def test_fixture_allows_only_documented_build_options_with_fixed_policy(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-single-platform-test-",
        ) as temporary:
            root = Path(temporary)
            cases = (
                (
                    "split",
                    [
                        "--platform",
                        "linux/amd64",
                        "--builder",
                        "default",
                        "--cpu-period",
                        "1000",
                        "--cpu-quota",
                        "1000000000",
                        "--memory",
                        "1024g",
                        "--no-cache",
                    ],
                ),
                (
                    "equals",
                    [
                        "--platform=linux/amd64",
                        "--builder=default",
                        "--cpu-period=1000000",
                        "--cpu-quota=1",
                        "--memory=1099511627776",
                        "--no-cache",
                    ],
                ),
            )
            for index, (name, arguments) in enumerate(cases):
                with self.subTest(name=name):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        [str(FIXTURE_BUILDER), *arguments],
                        fake.environment(
                            FAKE_DEFAULT_ATTESTATION_INDEX="1",
                            FAKE_IID_MODE="containerd",
                            XENOTEER_IMAGE=EXACT_IMAGE_ID,
                        ),
                    )

                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    build = next(
                        call for call in fake.calls() if call[0] == "build"
                    )
                    self.assertIn("--provenance=false", build)
                    self.assertIn("--sbom=false", build)
                    self.assertIn("--no-cache", build)
                    self.assertTrue(
                        any(
                            argument == "--platform"
                            or argument.startswith("--platform=")
                            for argument in build
                        ),
                        build,
                    )

    def test_fake_build_parser_models_context_alias_and_option_arity(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fake-build-parser-test-",
        ) as temporary:
            root = Path(temporary)

            rejected_commands = (
                (
                    "bare-terminator",
                    ["docker", "build", "--", "--tag", "hidden", "."],
                ),
                (
                    "extra-context",
                    [
                        "docker",
                        "build",
                        "--tag",
                        "xenoteer:extra-context",
                        ".",
                        "other",
                    ],
                ),
            )
            for index, (name, command) in enumerate(rejected_commands):
                with self.subTest(name=name):
                    case_root = root / f"reject-{index}"
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        command,
                        fake.environment(),
                    )

                    self.assertNotEqual(completed.returncode, 0)
                    state = json.loads(fake.state.read_text(encoding="utf-8"))
                    self.assertEqual(state["images"], {})

            with self.subTest(name="short-tag"):
                case_root = root / "short-tag"
                case_root.mkdir()
                fake = FakeDockerCase(case_root)
                completed = self.run_command(
                    [
                        "docker",
                        "build",
                        "-t",
                        "xenoteer:short-tag",
                        ".",
                    ],
                    fake.environment(),
                )

                self.assertEqual(completed.returncode, 0, completed.stderr)
                state = json.loads(fake.state.read_text(encoding="utf-8"))
                self.assertEqual(
                    state["images"]["xenoteer:short-tag"],
                    DERIVED_IMAGE_ID,
                )

            with self.subTest(name="short-output-changes-export"):
                case_root = root / "short-output"
                case_root.mkdir()
                fake = FakeDockerCase(case_root)
                completed = self.run_command(
                    [
                        "docker",
                        "build",
                        "-t",
                        "xenoteer:short-output",
                        "-o",
                        "type=local,dest=/tmp/fake-output",
                        ".",
                    ],
                    fake.environment(),
                )

                self.assertEqual(completed.returncode, 0, completed.stderr)
                state = json.loads(fake.state.read_text(encoding="utf-8"))
                self.assertEqual(state["images"], {})
                self.assertEqual(
                    state["build_outputs"],
                    ["type=local,dest=/tmp/fake-output"],
                )

            with self.subTest(name="array-tags-and-last-scalar-iidfile"):
                case_root = root / "arrays-scalars"
                case_root.mkdir()
                fake = FakeDockerCase(case_root)
                first_iid = case_root / "first.iid"
                last_iid = case_root / "last.iid"
                completed = self.run_command(
                    [
                        "docker",
                        "build",
                        "--tag",
                        "xenoteer:first-tag",
                        "-txenoteer:second-tag",
                        "--iidfile",
                        str(first_iid),
                        f"--iidfile={last_iid}",
                        ".",
                    ],
                    fake.environment(),
                )

                self.assertEqual(completed.returncode, 0, completed.stderr)
                state = json.loads(fake.state.read_text(encoding="utf-8"))
                self.assertEqual(
                    state["images"],
                    {
                        "xenoteer:first-tag": DERIVED_IMAGE_ID,
                        "xenoteer:second-tag": DERIVED_IMAGE_ID,
                    },
                )
                self.assertFalse(first_iid.exists())
                self.assertEqual(
                    last_iid.read_text(encoding="ascii"),
                    DERIVED_IMAGE_ID + "\n",
                )

    def test_docker_normalized_repo_tags_preserve_proven_identity(self) -> None:
        cases = (
            (
                "qualified-to-familiar",
                "docker.io/library/xenoteer-normalized:qualified",
                "familiar",
            ),
            (
                "implicit-to-latest",
                "xenoteer-normalized",
                "implicit-latest",
            ),
        )
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-normalized-repo-tags-test-",
        ) as temporary:
            root = Path(temporary)
            for index, (name, output_reference, mode) in enumerate(cases):
                with self.subTest(name=name):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        [str(NOVNC_GATE)],
                        fake.environment(
                            FAKE_IID_MODE="containerd",
                            FAKE_OUTPUT_REPOTAGS_MODE=mode,
                            XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                            XENOTEER_NOVNC_SPIKE_IMAGE=output_reference,
                        ),
                    )

                    calls = fake.calls()
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    self.assertTrue(
                        any(
                            call[:4]
                            == [
                                "image",
                                "inspect",
                                EXACT_IMAGE_ID,
                                output_reference,
                            ]
                            for call in calls
                        ),
                        calls,
                    )
                    runs = [call for call in calls if call[0] == "run"]
                    self.assertEqual(len(runs), 1)
                    self.assertEqual(runs[0][-1], MANIFEST_IMAGE_ID)

    def test_containerd_iid_rejects_unsafe_descriptor_and_tag_metadata(
        self,
    ) -> None:
        failures = (
            (
                {"FAKE_DESCRIPTOR_CONFIG_DIGEST": FOREIGN_IMAGE_ID},
                "config digest does not match build IID",
            ),
            (
                {"FAKE_DESCRIPTOR_DIGEST_MISMATCH": "1"},
                "manifest descriptor changed identity",
            ),
            (
                {"FAKE_DESCRIPTOR_MEDIA_TYPE": "text/plain"},
                "unsupported tagged output manifest media type",
            ),
            (
                {
                    "FAKE_DESCRIPTOR_MEDIA_TYPE": (
                        "application/vnd.oci.image.index.v1+json"
                    )
                },
                "disable BuildKit provenance and SBOM attestations",
            ),
            (
                {
                    "FAKE_DESCRIPTOR_MEDIA_TYPE": (
                        "application/vnd.docker.distribution."
                        "manifest.list.v2+json"
                    )
                },
                "disable BuildKit provenance and SBOM attestations",
            ),
            (
                {"FAKE_DESCRIPTOR_OMIT": "1"},
                "omitted its config-linked manifest descriptor",
            ),
            (
                {"FAKE_DESCRIPTOR_SIZE_INVALID": "1"},
                "manifest descriptor has an invalid size",
            ),
            (
                {"FAKE_DESCRIPTOR_ANNOTATIONS_INVALID": "1"},
                "manifest descriptor annotations are malformed",
            ),
            (
                {"FAKE_DESCRIPTOR_CONFIG_TYPE_INVALID": "1"},
                "manifest descriptor annotations are malformed",
            ),
            (
                {"FAKE_RETAG_OUTPUT_BEFORE_PROOF": "1"},
                "config digest does not match build IID",
            ),
            *(
                (
                    {"FAKE_OUTPUT_REPOTAGS_MODE": mode},
                    "malformed or dangling RepoTags metadata",
                )
                for mode in ("null", "empty", "non-string", "dangling")
            ),
            (
                {"FAKE_DERIVATION_METADATA_EXTRA": "1"},
                "exactly two image records",
            ),
            (
                {"FAKE_DERIVATION_METADATA_OVERSIZE": "1"},
                "metadata exceeded its size bound",
            ),
        )
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-containerd-iid-rejection-test-",
        ) as temporary:
            root = Path(temporary)
            for index, (overrides, expected_error) in enumerate(failures):
                with self.subTest(overrides=overrides):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    completed = self.run_command(
                        [str(NOVNC_GATE)],
                        fake.environment(
                            **overrides,
                            FAKE_IID_MODE="containerd",
                            XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                            XENOTEER_NOVNC_SPIKE_IMAGE=(
                                f"xenoteer:containerd-reject-{index}"
                            ),
                        ),
                    )

                    self.assertNotEqual(completed.returncode, 0)
                    self.assertIn(expected_error, completed.stderr)
                    self.assertFalse(
                        any(call[0] == "run" for call in fake.calls()),
                        fake.calls(),
                    )

    def test_derivation_requires_a_distinct_expected_output_reference(
        self,
    ) -> None:
        cases = (
            (
                "missing",
                "",
                "invalid expected local build output reference",
            ),
            (
                "base",
                '"$XENOTEER_LOCAL_IMAGE_ID"',
                "local build output reference conflicts with its exact base",
            ),
            (
                "alias",
                '"$XENOTEER_LOCAL_IMAGE_ALIAS"',
                "local build output reference conflicts with its exact base",
            ),
        )
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-expected-output-reference-test-",
        ) as temporary:
            root = Path(temporary)
            for index, (name, argument, expected_error) in enumerate(cases):
                with self.subTest(name=name):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    output_tag = f"xenoteer:expected-output-{index}"
                    driver = self.helper_driver(
                        case_root,
                        f"""
                        xenoteer_create_local_image_alias \
                            {EXACT_IMAGE_ID} expected-output
                        xenoteer_prepare_local_image_iidfile
                        docker build \
                            --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
                            --tag {output_tag} .
                        if xenoteer_verify_local_image_derivation \
                                {argument}; then
                            printf '%s\\n' \
                                'unsafe expected output reference was accepted' \
                                >&2
                            exit 91
                        fi
                        """,
                    )
                    completed = self.run_command(
                        [str(driver)],
                        fake.environment(),
                    )

                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    self.assertIn(expected_error, completed.stderr)
                    self.assertFalse(
                        any(
                            call[:4]
                            == [
                                "image",
                                "inspect",
                                EXACT_IMAGE_ID,
                                output_tag,
                            ]
                            for call in fake.calls()
                        ),
                        fake.calls(),
                    )
                    self.assertFalse(
                        any(call[0] == "run" for call in fake.calls()),
                        fake.calls(),
                    )
                    self.assert_fake_reservations_removed(fake)

    def test_browser_raw_local_id_is_aliased_before_docker_build(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-browser-local-id-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(BROWSER_GATE), EXACT_IMAGE_ID, "xenoteer:browser-test"],
                fake.environment(),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            build = next(call for call in calls if call[0] == "build")
            self.assertNotIn(
                f"XENOTEER_RUNTIME_IMAGE={EXACT_IMAGE_ID}",
                build,
            )
            self.assertTrue(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertTrue(any(call[:2] == ["image", "rm"] for call in calls))
            self.assertTrue(
                any(
                    call[:4]
                    == [
                        "image",
                        "inspect",
                        EXACT_IMAGE_ID,
                        "xenoteer:browser-test",
                    ]
                    for call in calls
                )
            )

    def test_novnc_freezes_iidfile_id_before_post_proof_consumers(self) -> None:
        output_tag = "xenoteer:novnc-retag-test"
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-novnc-iidfile-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_RETAG_OUTPUT_AFTER_PROOF="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                    XENOTEER_NOVNC_SPIKE_IMAGE=output_tag,
                ),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            build = next(call for call in calls if call[0] == "build")
            self.assertIn("--iidfile", build)
            proof_index = next(
                index
                for index, call in enumerate(calls)
                if call[:4]
                == ["image", "inspect", EXACT_IMAGE_ID, output_tag]
            )
            consumers = calls[proof_index + 1 :]
            self.assertTrue(
                all(
                    output_tag not in call
                    for call in consumers
                    if call[:2] == ["image", "inspect"] or call[0] == "run"
                ),
                consumers,
            )
            self.assertTrue(
                all(call[-1] == DERIVED_IMAGE_ID for call in consumers if call[0] == "run"),
                consumers,
            )

    def test_browser_freezes_iidfile_id_for_both_runtime_profiles(self) -> None:
        output_tag = "xenoteer:browser-retag-test"
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-browser-iidfile-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(BROWSER_GATE), EXACT_IMAGE_ID, output_tag],
                fake.environment(FAKE_RETAG_OUTPUT_AFTER_PROOF="1"),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            build = next(call for call in calls if call[0] == "build")
            self.assertIn("--iidfile", build)
            runs = [call for call in calls if call[0] == "run"]
            self.assertEqual(len(runs), 2)
            self.assertTrue(all(call[-1] == DERIVED_IMAGE_ID for call in runs), runs)

    def test_fixture_uses_iidfile_id_before_label_and_derivation_checks(self) -> None:
        output_tag = "xenoteer:fixture-retag-test"
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-iidfile-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(FIXTURE_BUILDER)],
                fake.environment(
                    FAKE_RETAG_OUTPUT_AFTER_PROOF="1",
                    XENOTEER_DESKTOP_APPS_IMAGE=output_tag,
                    XENOTEER_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            build = next(call for call in calls if call[0] == "build")
            self.assertIn("--iidfile", build)
            proof_index = next(
                index
                for index, call in enumerate(calls)
                if call[:4]
                == ["image", "inspect", EXACT_IMAGE_ID, output_tag]
            )
            self.assertTrue(
                all(
                    output_tag not in call
                    for call in calls[proof_index + 1 :]
                    if call[:2] == ["image", "inspect"]
                ),
                calls[proof_index + 1 :],
            )
            label_inspects = [
                call
                for call in calls[proof_index + 1 :]
                if call[:2] == ["image", "inspect"]
                and any("fixture." in argument for argument in call)
            ]
            self.assertEqual(len(label_inspects), 2)
            self.assertTrue(
                all(call[2] == DERIVED_IMAGE_ID for call in label_inspects),
                label_inspects,
            )

    def test_iidfile_is_absent_in_private_reservation_before_docker_build(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-absent-iidfile-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_REQUIRE_ABSENT_IID="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_docker_recreated_iidfile_is_accepted_and_exact(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-recreated-iidfile-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_RECREATE_IID="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertTrue(
                any(
                    call[:4]
                    == [
                        "image",
                        "inspect",
                        EXACT_IMAGE_ID,
                        "xenoteer:novnc-spike",
                    ]
                    for call in calls
                ),
                calls,
            )
            runs = [call for call in calls if call[0] == "run"]
            self.assertEqual(len(runs), 1)
            self.assertEqual(runs[0][-1], DERIVED_IMAGE_ID)

    def test_safe_docker_iid_modes_are_reduced_to_0600(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-private-docker-iid-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"""
                xenoteer_create_local_image_alias {EXACT_IMAGE_ID} iid-mode
                xenoteer_prepare_local_image_iidfile
                docker build --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
                    --tag xenoteer:iid-mode-test .
                for docker_mode in 644 640 440; do
                    chmod "$docker_mode" "$XENOTEER_LOCAL_IMAGE_IIDFILE"
                    xenoteer_verify_local_image_derivation \
                        xenoteer:iid-mode-test
                    test "$(/usr/bin/stat -c %a \
                        "$XENOTEER_LOCAL_IMAGE_IIDFILE")" = 600
                done
                """,
            )
            completed = self.run_command(
                [str(driver)],
                fake.environment(),
            )
            self.assertEqual(
                completed.returncode,
                0,
                completed.stderr,
            )
            self.assert_fake_reservations_removed(fake)

    def test_preexisting_iid_path_is_rejected_before_docker_build(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-preexisting-docker-iid-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"""
                xenoteer_create_local_image_alias {EXACT_IMAGE_ID} iid-present
                printf hostile >"$XENOTEER_LOCAL_IMAGE_IIDFILE"
                xenoteer_prepare_local_image_iidfile
                docker build --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
                    --tag xenoteer:iid-present-test .
                """,
            )
            completed = self.run_command([str(driver)], fake.environment())
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[0] == "build" for call in fake.calls()))
            self.assert_fake_reservations_removed(fake)

    def test_iidfile_special_or_mutated_files_fail_before_downstream_use(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-mutated-iidfile-test-",
        ) as temporary:
            root = Path(temporary)
            for mutation in ("fifo", "symlink", "hardlink", "oversize", "mode"):
                with self.subTest(mutation=mutation):
                    case_root = root / mutation
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    driver = self.helper_driver(
                        case_root,
                        f"""
                        xenoteer_create_local_image_alias \
                            {EXACT_IMAGE_ID} iid-mutation
                        xenoteer_prepare_local_image_iidfile
                        xenoteer_run_guarded_local_image_command docker build \
                            --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
                            --tag xenoteer:iid-mutation-test .
                        xenoteer_verify_local_image_derivation \
                            xenoteer:iid-mutation-test
                        """,
                    )
                    completed = self.run_command_in_killable_group(
                        [str(driver)],
                        fake.environment(
                            FAKE_IID_MUTATION=mutation,
                        ),
                    )
                    self.assertNotEqual(
                        completed.returncode,
                        124,
                        completed.stderr,
                    )
                    self.assertNotEqual(completed.returncode, 0)
                    calls = fake.calls()
                    self.assertFalse(
                        any(
                            call[:4]
                            == [
                                "image",
                                "inspect",
                                EXACT_IMAGE_ID,
                                DERIVED_IMAGE_ID,
                            ]
                            for call in calls
                        ),
                        calls,
                    )
                    self.assertFalse(any(call[0] == "run" for call in calls))
                    self.assert_fake_reservations_removed(fake)

    def test_reservation_owner_record_mismatch_fails_before_derivation_inspect(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-iid-owner-mismatch-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            reservation_record = root / "owner-reservation-path"
            driver = self.helper_driver(
                root,
                f"""
                xenoteer_create_local_image_alias {EXACT_IMAGE_ID} iid-owner
                xenoteer_prepare_local_image_iidfile
                docker build --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
                    --tag xenoteer:iid-owner-test .
                printf '%s\\n' "$XENOTEER_LOCAL_IMAGE_RESERVATION" >\
{shlex.quote(str(reservation_record))}
                XENOTEER_LOCAL_IMAGE_RESERVATION_UID=$((XENOTEER_LOCAL_IMAGE_RESERVATION_UID + 1))
                xenoteer_verify_local_image_derivation \
                    xenoteer:iid-owner-test
                """,
            )
            completed = self.run_command([str(driver)], fake.environment())
            reservation_lines = reservation_record.read_text(
                encoding="ascii"
            ).splitlines()
            reservation_candidate = (
                Path(reservation_lines[0]) if reservation_lines else None
            )
            if (
                reservation_candidate is not None
                and re.fullmatch(
                    r"/tmp/xenoteer-local-image-[0-9a-f]{32}",
                    str(reservation_candidate),
                )
                is not None
            ):
                self.addCleanup(
                    self.securely_remove_exact_test_reservation,
                    reservation_candidate,
                    require_residue=False,
                )
            self.assertEqual(len(reservation_lines), 1, reservation_lines)
            self.assertIsNotNone(
                reservation_candidate,
                reservation_lines,
            )
            assert reservation_candidate is not None
            reservation = reservation_candidate

            self.assertNotEqual(completed.returncode, 0, completed.stderr)
            self.assertFalse(
                any(
                    call[:4]
                    == [
                        "image",
                        "inspect",
                        EXACT_IMAGE_ID,
                        "xenoteer:iid-owner-test",
                    ]
                    for call in fake.calls()
                )
            )
            self.assertTrue(os.path.lexists(reservation))
            self.assertTrue(
                os.path.lexists(reservation / "derived-image-id")
            )
            self.securely_remove_exact_test_reservation(
                reservation,
                require_residue=True,
            )
            self.assertFalse(os.path.lexists(reservation))

    def test_fixture_collision_never_retags_or_removes_foreign_alias(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-alias-collision-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(FIXTURE_BUILDER)],
                fake.environment(
                    FAKE_COLLISION="1",
                    XENOTEER_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))

    def test_fixture_uses_alias_and_proves_exact_distinct_derivation(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-local-id-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(FIXTURE_BUILDER)],
                fake.environment(
                    XENOTEER_DESKTOP_APPS_IMAGE="xenoteer:fixture-test",
                    XENOTEER_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            build = next(call for call in calls if call[0] == "build")
            self.assertNotIn(
                f"XENOTEER_BASE_IMAGE={EXACT_IMAGE_ID}",
                build,
            )
            self.assertIn(
                f"XENOTEER_FIXTURE_BASE_IMAGE_ID={EXACT_IMAGE_ID}",
                build,
            )
            self.assertTrue(
                any(
                    call[:4]
                    == [
                        "image",
                        "inspect",
                        EXACT_IMAGE_ID,
                        "xenoteer:fixture-test",
                    ]
                    for call in calls
                )
            )
            self.assertTrue(any(call[:2] == ["image", "rm"] for call in calls))

    def test_fixture_rejects_derived_output_equal_to_base(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-fixture-same-id-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(FIXTURE_BUILDER)],
                fake.environment(
                    FAKE_DERIVED_IS_BASE="1",
                    XENOTEER_DESKTOP_APPS_IMAGE="xenoteer:fixture-test",
                    XENOTEER_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn(
                "unexpectedly resolves to the exact base",
                completed.stderr,
            )

    def test_malformed_source_id_fails_before_tag_or_build(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-malformed-local-id-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_MALFORMED_SOURCE="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertFalse(any(call[0] == "build" for call in calls))

    def test_dangling_source_is_rejected_before_alias_can_be_last_reference(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-dangling-local-source-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_DELETE_LAST_REFERENCE="1",
                    FAKE_SOURCE_METADATA_MODE="dangling",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))
            self.assertFalse(any(call[0] == "build" for call in calls))
            state = json.loads(fake.state.read_text(encoding="utf-8"))
            self.assertTrue(state["source_present"])

    def test_malformed_source_metadata_fails_before_tag_or_removal(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-malformed-source-metadata-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_SOURCE_METADATA_MALFORMED="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))

    def test_source_metadata_daemon_failure_fails_before_tag_or_removal(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-source-metadata-daemon-failure-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_SOURCE_METADATA_FAIL="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[:2] == ["image", "tag"] for call in calls))
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))

    def test_tagged_or_digest_source_survives_owned_alias_cleanup(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-durable-local-source-test-",
        ) as temporary:
            root = Path(temporary)
            for metadata_mode in ("tagged", "digest"):
                with self.subTest(metadata=metadata_mode):
                    case_root = root / metadata_mode
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    driver = self.helper_driver(
                        case_root,
                        f"xenoteer_create_local_image_alias "
                        f"{EXACT_IMAGE_ID} durable-source",
                    )
                    completed = self.run_command(
                        [str(driver)],
                        fake.environment(
                            FAKE_DELETE_LAST_REFERENCE="1",
                            FAKE_SOURCE_METADATA_MODE=metadata_mode,
                        ),
                    )
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    calls = fake.calls()
                    self.assertTrue(
                        any(
                            call[:3] == ["image", "inspect", EXACT_IMAGE_ID]
                            and "RepoTags" in " ".join(call)
                            and "RepoDigests" in " ".join(call)
                            for call in calls
                        ),
                        calls,
                    )
                    self.assertTrue(
                        any(call[:2] == ["image", "rm"] for call in calls)
                    )
                    state = json.loads(fake.state.read_text(encoding="utf-8"))
                    self.assertTrue(state["source_present"])

    def test_tag_failure_never_builds_or_removes_unowned_alias(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-tag-failure-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_TAG_FAIL="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertEqual(completed.returncode, 1)
            self.assertFalse(any(call[0] == "build" for call in calls))
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))

    def test_daemon_failure_during_absence_probe_never_tags(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-probe-failure-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_ALIAS_ABSENCE_PROBE_FAIL="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertTrue(any(call[:2] == ["image", "ls"] for call in calls))
            self.assertFalse(any(call[:2] == ["image", "tag"] for call in calls))

    def test_build_failure_survives_cleanup_failure(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-build-failure-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_BUILD_FAIL="42",
                    FAKE_RM_FAIL="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            self.assertEqual(completed.returncode, 42, completed.stderr)
            self.assertTrue(
                any(call[:2] == ["image", "rm"] for call in fake.calls())
            )

    def test_cleanup_failure_turns_success_into_failure(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-cleanup-failure-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"xenoteer_create_local_image_alias "
                f"{EXACT_IMAGE_ID} cleanup-failure",
            )
            completed = self.run_command(
                [str(driver)],
                fake.environment(
                    FAKE_RM_FAIL="1",
                ),
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)

    def test_post_build_alias_drift_is_rejected_and_not_removed(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-alias-drift-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_ALIAS_DRIFT_AT="3",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))

    def test_pre_build_alias_drift_is_rejected_before_build(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-prebuild-drift-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_ALIAS_DRIFT_AT="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            calls = fake.calls()
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(any(call[0] == "build" for call in calls))
            self.assertFalse(any(call[:2] == ["image", "rm"] for call in calls))

    def test_bad_derived_layer_prefix_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-layer-prefix-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_BAD_LAYER_PREFIX="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertTrue(
                any(call[:2] == ["image", "rm"] for call in fake.calls())
            )

    def test_derivation_inspect_failure_cannot_be_masked_by_valid_json(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-derivation-inspect-failure-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_DERIVATION_INSPECT_FAIL="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            self.assertNotEqual(completed.returncode, 0)

    def test_derived_output_must_not_resolve_to_base_image(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-same-derived-id-test-",
        ) as temporary:
            fake = FakeDockerCase(Path(temporary))
            completed = self.run_command(
                [str(NOVNC_GATE)],
                fake.environment(
                    FAKE_DERIVED_IS_BASE="1",
                    XENOTEER_NOVNC_SPIKE_BASE_IMAGE=EXACT_IMAGE_ID,
                ),
            )
            self.assertNotEqual(completed.returncode, 0)

    def test_source_reference_loss_before_cleanup_retains_owned_alias(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-source-survival-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"xenoteer_create_local_image_alias "
                f"{EXACT_IMAGE_ID} source-loss",
            )
            completed = self.run_command(
                [str(driver)],
                fake.environment(
                    FAKE_SOURCE_METADATA_DANGLING_AFTER="1",
                ),
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            self.assertFalse(
                any(call[:2] == ["image", "rm"] for call in fake.calls())
            )
            state = json.loads(fake.state.read_text(encoding="utf-8"))
            self.assertEqual(len(state["aliases"]), 1)
            self.assertIn("durable", completed.stderr)

    def test_source_metadata_failure_before_cleanup_retains_owned_alias(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-source-cleanup-metadata-failure-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"xenoteer_create_local_image_alias "
                f"{EXACT_IMAGE_ID} source-metadata-failure",
            )
            completed = self.run_command(
                [str(driver)],
                fake.environment(
                    FAKE_SOURCE_METADATA_FAIL_AFTER="1",
                ),
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            self.assertFalse(
                any(call[:2] == ["image", "rm"] for call in fake.calls())
            )
            state = json.loads(fake.state.read_text(encoding="utf-8"))
            self.assertEqual(len(state["aliases"]), 1)

    def test_signals_preserve_status_and_clean_owned_alias(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-signal-test-",
        ) as temporary:
            root = Path(temporary)
            for signal_name, expected in (("HUP", 129), ("INT", 130), ("TERM", 143)):
                with self.subTest(signal=signal_name):
                    case_root = root / signal_name.lower()
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    driver = self.helper_driver(
                        case_root,
                        f"""
                        xenoteer_create_local_image_alias {EXACT_IMAGE_ID} signal
                        kill -{signal_name} $$
                        """,
                    )
                    completed = self.run_command([str(driver)], fake.environment())
                    self.assertEqual(
                        completed.returncode,
                        expected,
                        completed.stderr,
                    )
                    self.assertTrue(
                        any(
                            call[:2] == ["image", "rm"]
                            for call in fake.calls()
                        )
                    )

    def test_foreground_build_and_runtime_signals_are_bounded_and_reaped(
        self,
    ) -> None:
        for block_on in ("build", "run"):
            for signal_number, expected in (
                (signal.SIGHUP, 129),
                (signal.SIGINT, 130),
                (signal.SIGTERM, 143),
            ):
                with self.subTest(
                    operation=block_on,
                    signal=signal_number.name,
                ):
                    self.assert_signal_stops_guarded_docker(
                        block_on=block_on,
                        driver_body=(
                            f"""
                            xenoteer_create_local_image_alias \
                                {EXACT_IMAGE_ID} guarded-build
                            xenoteer_prepare_local_image_iidfile
                            xenoteer_run_guarded_local_image_command \
                                docker build \
                                --iidfile "$XENOTEER_LOCAL_IMAGE_IIDFILE" \
                                --tag xenoteer:guarded-build-test .
                            """
                            if block_on == "build"
                            else None
                        ),
                        signal_number=signal_number,
                        expected_status=expected,
                    )

    def test_browser_detached_run_is_pretracked_for_signal_cleanup(self) -> None:
        # The shared signal wrapper's HUP/INT/TERM status matrix is exercised
        # above for both build and runtime children. This case isolates the
        # browser-specific invariant: pre-track a detached container before
        # entering the guarded client call.
        self.assert_signal_stops_guarded_docker(
            block_on="run",
            command=[
                str(BROWSER_GATE),
                EXACT_IMAGE_ID,
                "xenoteer:browser-signal-test",
            ],
            signal_number=signal.SIGTERM,
            expected_status=143,
        )

    def test_signal_after_tag_before_return_still_cleans_alias(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-tag-signal-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"xenoteer_create_local_image_alias {EXACT_IMAGE_ID} signal-gap",
            )
            completed = self.run_command(
                [str(driver)],
                fake.environment(FAKE_SIGNAL_DURING_TAG="1"),
            )
            self.assertEqual(completed.returncode, 143, completed.stderr)
            self.assertTrue(
                any(call[:2] == ["image", "rm"] for call in fake.calls())
            )

    def test_hostile_alias_purpose_fails_before_docker_tag(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-hostile-purpose-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"xenoteer_create_local_image_alias {EXACT_IMAGE_ID} '../bad'",
            )
            completed = self.run_command([str(driver)], fake.environment())
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(
                any(call[:2] == ["image", "tag"] for call in fake.calls())
            )

    def test_hostile_source_is_rejected_before_docker(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-hostile-source-test-",
        ) as temporary:
            root = Path(temporary)
            hostile_expressions = (
                ("option-leading", "'--hostile'"),
                ("newline", '"$(printf \'bad\\nreference\')"'),
            )
            for index, (label, expression) in enumerate(hostile_expressions):
                with self.subTest(hostile=label):
                    case_root = root / str(index)
                    case_root.mkdir()
                    fake = FakeDockerCase(case_root)
                    driver = self.helper_driver(
                        case_root,
                        "xenoteer_create_local_image_alias "
                        f"{expression} hostile-source",
                    )
                    completed = self.run_command(
                        [str(driver)],
                        fake.environment(),
                    )
                    self.assertNotEqual(completed.returncode, 0)
                    self.assertEqual(fake.calls(), [])

    def test_concurrent_aliases_are_unique_and_both_are_cleaned(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="xenoteer-local-concurrency-test-",
        ) as temporary:
            root = Path(temporary)
            fake = FakeDockerCase(root)
            driver = self.helper_driver(
                root,
                f"""
                xenoteer_create_local_image_alias {EXACT_IMAGE_ID} concurrent
                xenoteer_verify_local_image_alias
                /usr/bin/sleep 0.05
                """,
            )
            processes = [
                subprocess.Popen(
                    [str(driver)],
                    cwd=REPOSITORY_ROOT,
                    env=fake.environment(),
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                for _ in range(2)
            ]
            results = [
                process.communicate(timeout=8)
                for process in processes
            ]
            self.assertEqual([process.returncode for process in processes], [0, 0], results)
            tags = [
                call[3]
                for call in fake.calls()
                if call[:2] == ["image", "tag"]
            ]
            self.assertEqual(len(tags), 2)
            self.assertEqual(len(set(tags)), 2)
            removals = [
                call[2]
                for call in fake.calls()
                if call[:2] == ["image", "rm"]
            ]
            self.assertCountEqual(removals, tags)


if __name__ == "__main__":
    unittest.main()
