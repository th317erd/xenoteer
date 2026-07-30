#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify that Xenoteer's public Rust packages remain Apache-only boundaries."""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from collections.abc import Callable, Mapping, Sequence
from typing import Any


APACHE_LICENSE = "Apache-2.0"
COMMAND_TIMEOUT_SECONDS = 30
GENERATED_PACKAGE_ENTRIES = frozenset(
    {
        ".cargo_vcs_info.json",
        "Cargo.lock",
        "Cargo.toml.orig",
    }
)
REQUIRED_PACKAGE_ENTRIES = frozenset({"LICENSE", "NOTICE"})
REVIEWED_PACKAGE_EXAMPLES = {
    "xenoteer-protocol": frozenset(),
    "xenoteer-sdk": frozenset({"examples/phase6_behaviors.rs"}),
}
RUST_SOURCE_SUFFIX = ".rs"
SPDX_MARKER = "SPDX-License-Identifier:"
BUSL_TEXT_MARKERS = (
    "Business Source License 1.1",
    "SPDX-License-Identifier: BUSL-1.1",
    "SPDX-License-Identifier: BSL-1.1",
)


class BoundaryError(RuntimeError):
    """One fail-closed public package-boundary violation."""


@dataclasses.dataclass(frozen=True)
class Boundary:
    """Expected source and local dependency closure for one public package."""

    package_name: str
    package_root: pathlib.Path
    allowed_local_packages: frozenset[str]


def boundary_specs(repo_root: pathlib.Path) -> tuple[Boundary, ...]:
    """Return the canonical, deliberately small public-package allowlists."""

    crates_root = repo_root.resolve() / "crates"
    return (
        Boundary(
            package_name="xenoteer-protocol",
            package_root=crates_root / "xenoteer-protocol",
            allowed_local_packages=frozenset({"xenoteer-protocol"}),
        ),
        Boundary(
            package_name="xenoteer-sdk",
            package_root=crates_root / "xenoteer-sdk",
            allowed_local_packages=frozenset(
                {"xenoteer-protocol", "xenoteer-sdk"}
            ),
        ),
    )


def parse_metadata(output: str) -> dict[str, Any]:
    """Parse Cargo metadata while turning malformed output into a stable error."""

    try:
        metadata = json.loads(output)
    except json.JSONDecodeError as error:
        raise BoundaryError(f"cargo metadata returned invalid JSON: {error}") from error
    if not isinstance(metadata, dict):
        raise BoundaryError("cargo metadata must be a top-level object")
    return metadata


def _object_list(value: object, field: str) -> list[dict[str, Any]]:
    if not isinstance(value, list) or not all(
        isinstance(item, dict) for item in value
    ):
        raise BoundaryError(f"cargo metadata field {field!r} must be an object list")
    return value


def _string_list(value: object, field: str) -> list[str]:
    if not isinstance(value, list) or not all(
        isinstance(item, str) for item in value
    ):
        raise BoundaryError(f"cargo metadata field {field!r} must be a string list")
    return value


def _package_name(package: Mapping[str, Any]) -> str:
    name = package.get("name")
    if not isinstance(name, str) or not name:
        raise BoundaryError("cargo metadata package has no non-empty name")
    return name


def _package_id(package: Mapping[str, Any]) -> str:
    package_id = package.get("id")
    if not isinstance(package_id, str) or not package_id:
        raise BoundaryError("cargo metadata package has no non-empty id")
    return package_id


def _manifest_parent(package: Mapping[str, Any]) -> pathlib.Path:
    manifest_path = package.get("manifest_path")
    if not isinstance(manifest_path, str) or not manifest_path:
        raise BoundaryError(
            f"cargo metadata package {_package_name(package)} has no manifest path"
        )
    return pathlib.Path(manifest_path).resolve().parent


def validate_dependency_closures(
    metadata: Mapping[str, Any],
    repo_root: pathlib.Path,
) -> dict[str, tuple[str, ...]]:
    """Validate all-feature resolved local dependencies for both public crates."""

    packages = _object_list(metadata.get("packages"), "packages")
    workspace_members = set(
        _string_list(metadata.get("workspace_members"), "workspace_members")
    )
    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict):
        raise BoundaryError("cargo metadata did not include a resolve graph")
    nodes = _object_list(resolve.get("nodes"), "resolve.nodes")

    packages_by_id: dict[str, dict[str, Any]] = {}
    for package in packages:
        package_id = _package_id(package)
        if package_id in packages_by_id:
            raise BoundaryError(f"duplicate cargo metadata package id: {package_id}")
        packages_by_id[package_id] = package

    nodes_by_id: dict[str, dict[str, Any]] = {}
    for node in nodes:
        node_id = node.get("id")
        if not isinstance(node_id, str) or not node_id:
            raise BoundaryError("cargo metadata resolve node has no non-empty id")
        if node_id in nodes_by_id:
            raise BoundaryError(f"duplicate cargo metadata resolve node id: {node_id}")
        nodes_by_id[node_id] = node

    boundaries = boundary_specs(repo_root)
    expected_roots = {
        boundary.package_name: boundary.package_root.resolve()
        for boundary in boundaries
    }
    results: dict[str, tuple[str, ...]] = {}

    for boundary in boundaries:
        root_candidates = [
            package
            for package in packages
            if _package_name(package) == boundary.package_name
            and _package_id(package) in workspace_members
        ]
        if len(root_candidates) != 1:
            raise BoundaryError(
                f"expected exactly one workspace package named "
                f"{boundary.package_name}, found {len(root_candidates)}"
            )
        root = root_candidates[0]
        root_id = _package_id(root)
        if _manifest_parent(root) != boundary.package_root.resolve():
            raise BoundaryError(
                f"{boundary.package_name} manifest escaped its expected package root"
            )

        pending = [root_id]
        visited: set[str] = set()
        local_names: set[str] = set()
        while pending:
            package_id = pending.pop()
            if package_id in visited:
                continue
            visited.add(package_id)
            package = packages_by_id.get(package_id)
            if package is None:
                raise BoundaryError(
                    f"resolve graph references unknown package id: {package_id}"
                )
            node = nodes_by_id.get(package_id)
            if node is None:
                raise BoundaryError(
                    f"resolve graph has no node for package {_package_name(package)}"
                )

            name = _package_name(package)
            license_expression = package.get("license")
            if (
                isinstance(license_expression, str)
                and "BUSL-1.1" in license_expression
            ):
                raise BoundaryError(
                    f"{license_expression} dependency {name} entered "
                    f"{boundary.package_name}'s closure"
                )

            if package.get("source") is None:
                if license_expression != APACHE_LICENSE:
                    raise BoundaryError(
                        f"{license_expression or 'missing license'} local dependency "
                        f"{name} entered {boundary.package_name}'s closure"
                    )
                if package_id not in workspace_members:
                    raise BoundaryError(
                        f"unexpected local package {name} outside the workspace "
                        f"entered {boundary.package_name}'s closure"
                    )
                if name not in boundary.allowed_local_packages:
                    raise BoundaryError(
                        f"unexpected local package {name} entered "
                        f"{boundary.package_name}'s closure"
                    )
                expected_root = expected_roots.get(name)
                if expected_root is None or _manifest_parent(package) != expected_root:
                    raise BoundaryError(
                        f"local package {name} does not use its canonical public root"
                    )
                local_names.add(name)

            dependencies = _object_list(node.get("deps"), f"resolve node {package_id}.deps")
            for dependency in dependencies:
                dependency_id = dependency.get("pkg")
                if not isinstance(dependency_id, str) or not dependency_id:
                    raise BoundaryError(
                        f"resolve node {package_id} has a dependency without a package id"
                    )
                pending.append(dependency_id)

        if local_names != set(boundary.allowed_local_packages):
            missing = sorted(set(boundary.allowed_local_packages) - local_names)
            raise BoundaryError(
                f"{boundary.package_name} local dependency closure is incomplete: "
                f"{', '.join(missing)}"
            )
        results[boundary.package_name] = tuple(sorted(local_names))

    return results


def validate_registry_publish_metadata(
    metadata: Mapping[str, Any],
    boundaries: Sequence[Boundary],
) -> None:
    """Require every public root crate to be publishable to crates.io."""

    packages = _object_list(metadata.get("packages"), "packages")
    for boundary in boundaries:
        matches = [
            package
            for package in packages
            if _package_name(package) == boundary.package_name
            and _manifest_parent(package) == boundary.package_root.resolve()
        ]
        if len(matches) != 1:
            raise BoundaryError(
                f"expected one public package manifest for {boundary.package_name}"
            )
        publish = matches[0].get("publish")
        if publish == [] or (
            publish is not None
            and (
                not isinstance(publish, list)
                or "crates-io" not in publish
            )
        ):
            raise BoundaryError(
                f"{boundary.package_name} must permit crates.io publish"
            )


def validate_packaged_manifest(package_name: str, payload: bytes) -> None:
    """Require normalized SDK dependencies to be registry-resolvable."""

    try:
        manifest = tomllib.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise BoundaryError(
            f"{package_name} packaged Cargo.toml is invalid: {error}"
        ) from error
    if package_name != "xenoteer-sdk":
        return
    dependencies = manifest.get("dependencies")
    protocol = (
        dependencies.get("xenoteer-protocol")
        if isinstance(dependencies, dict)
        else None
    )
    if (
        not isinstance(protocol, dict)
        or protocol.get("version") != "=0.1.0"
        or any(key in protocol for key in ("path", "git"))
    ):
        raise BoundaryError(
            "xenoteer-sdk packaged xenoteer-protocol dependency is not "
            "registry-resolvable at exactly 0.1.0"
        )


def validate_packaged_resolution_metadata(metadata: Mapping[str, Any]) -> None:
    """Validate Cargo's resolution view from the staged SDK package."""

    packages = _object_list(metadata.get("packages"), "packages")
    sdk_matches = [
        package for package in packages if package.get("name") == "xenoteer-sdk"
    ]
    protocol_matches = [
        package for package in packages if package.get("name") == "xenoteer-protocol"
    ]
    if len(sdk_matches) != 1 or len(protocol_matches) != 1:
        raise BoundaryError(
            "packaged SDK resolution did not contain exactly one SDK and protocol crate"
        )
    dependencies = _object_list(
        sdk_matches[0].get("dependencies"),
        "packaged SDK dependencies",
    )
    protocol_dependencies = [
        dependency
        for dependency in dependencies
        if dependency.get("name") == "xenoteer-protocol"
    ]
    if len(protocol_dependencies) != 1:
        raise BoundaryError(
            "packaged SDK resolution did not contain one protocol dependency"
        )
    dependency = protocol_dependencies[0]
    source = dependency.get("source")
    if (
        dependency.get("req") != "=0.1.0"
        or dependency.get("path") is not None
        or not isinstance(source, str)
        or not source.startswith("registry+")
    ):
        raise BoundaryError(
            "packaged SDK protocol dependency is not an exact registry dependency"
        )


def read_archive_file(
    archive: pathlib.Path,
    package_name: str,
    relative_name: str,
) -> bytes:
    """Read one already-validated regular member from a staged crate."""

    package_prefix = archive.name.removesuffix(".crate")
    member_name = f"{package_prefix}/{relative_name}"
    try:
        with tarfile.open(archive, mode="r:*") as crate:
            member = crate.getmember(member_name)
            source = crate.extractfile(member)
            if source is None:
                raise BoundaryError(
                    f"{package_name} archive member is not readable: {relative_name}"
                )
            return source.read()
    except (KeyError, tarfile.TarError, OSError) as error:
        raise BoundaryError(
            f"{package_name} archive is missing {relative_name}: {error}"
        ) from error


def extract_validated_archive(
    archive: pathlib.Path,
    package_name: str,
    destination: pathlib.Path,
) -> None:
    """Extract regular members after archive structure validation."""

    package_prefix = archive.name.removesuffix(".crate")
    destination.mkdir(parents=True, exist_ok=False)
    try:
        with tarfile.open(archive, mode="r:*") as crate:
            for member in crate.getmembers():
                member_path = pathlib.PurePosixPath(member.name)
                relative = pathlib.PurePosixPath(*member_path.parts[1:])
                _validate_safe_relative_path(relative.as_posix(), package_name)
                source = crate.extractfile(member)
                if source is None:
                    raise BoundaryError(
                        f"{package_name} archive member is not a file: {member.name}"
                    )
                output = destination.joinpath(*relative.parts)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(source.read())
    except (tarfile.TarError, OSError) as error:
        raise BoundaryError(
            f"{package_name} archive extraction failed: {error}"
        ) from error


def _validate_safe_relative_path(path: str, package_name: str) -> pathlib.PurePosixPath:
    if not path or "\x00" in path or "\\" in path:
        raise BoundaryError(f"{package_name} has an unsafe package path: {path!r}")
    relative = pathlib.PurePosixPath(path)
    if (
        relative.is_absolute()
        or any(part in {"", ".", ".."} for part in relative.parts)
        or relative.as_posix() != path
    ):
        raise BoundaryError(f"{package_name} has an unsafe package path: {path!r}")
    return relative


def _reject_symlink_components(
    package_root: pathlib.Path,
    relative: pathlib.PurePosixPath,
    package_name: str,
) -> None:
    candidate = package_root
    for component in relative.parts:
        candidate /= component
        if candidate.is_symlink():
            raise BoundaryError(
                f"{package_name} has symlinked package source: {relative.as_posix()}"
            )


def _busl_source_hashes(boundary: Boundary) -> frozenset[str]:
    """Hash exact Rust sources from sibling BUSL crates for copy detection."""

    crates_root = boundary.package_root.resolve().parent
    hashes: set[str] = set()
    for manifest_path in sorted(crates_root.glob("*/Cargo.toml")):
        try:
            manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
            raise BoundaryError(
                f"cannot inspect sibling crate license {manifest_path}: {error}"
            ) from error
        package = manifest.get("package")
        if not isinstance(package, dict):
            continue
        license_expression = package.get("license")
        if not isinstance(license_expression, str) or "BUSL-1.1" not in license_expression:
            continue
        source_root = manifest_path.parent
        for source in sorted(source_root.rglob(f"*{RUST_SOURCE_SUFFIX}")):
            if source.is_symlink() or not source.is_file():
                continue
            try:
                hashes.add(hashlib.sha256(source.read_bytes()).hexdigest())
            except OSError as error:
                raise BoundaryError(
                    f"cannot hash BUSL-1.1 source {source}: {error}"
                ) from error
    return frozenset(hashes)


def _validate_public_source_bytes(
    boundary: Boundary,
    relative: str,
    encoded: bytes,
    busl_hashes: frozenset[str],
) -> None:
    """Reject non-Apache markers and exact copies of private Rust sources."""

    if pathlib.PurePosixPath(relative).suffix != RUST_SOURCE_SUFFIX:
        return
    try:
        text = encoded.decode("utf-8")
    except UnicodeDecodeError as error:
        raise BoundaryError(
            f"{boundary.package_name} Rust source is not UTF-8: {relative}"
        ) from error
    for line in text.splitlines():
        if SPDX_MARKER not in line:
            continue
        expression = line.split(SPDX_MARKER, 1)[1].strip()
        if expression != APACHE_LICENSE:
            raise BoundaryError(
                f"{boundary.package_name} has non-Apache source license marker: "
                f"{relative}"
            )
    if any(marker in text for marker in BUSL_TEXT_MARKERS):
        raise BoundaryError(
            f"{boundary.package_name} has non-Apache source license marker: {relative}"
        )
    if hashlib.sha256(encoded).hexdigest() in busl_hashes:
        raise BoundaryError(
            f"{boundary.package_name} contains copied BUSL-1.1 source: {relative}"
        )


def validate_package_listing(boundary: Boundary, output: str) -> tuple[str, ...]:
    """Validate one deterministic Cargo archive listing against its source root."""

    entries = output.splitlines()
    if not entries or any(not entry for entry in entries):
        raise BoundaryError(
            f"{boundary.package_name} cargo package listing is empty or malformed"
        )
    if len(entries) != len(set(entries)):
        raise BoundaryError(
            f"{boundary.package_name} cargo package listing contains duplicates"
        )
    if entries != sorted(entries):
        raise BoundaryError(
            f"{boundary.package_name} cargo package listing is not canonically sorted"
        )

    missing = sorted(REQUIRED_PACKAGE_ENTRIES - set(entries))
    if missing:
        raise BoundaryError(
            f"{boundary.package_name} is missing required package entries: "
            f"{', '.join(missing)}"
        )

    expected_examples = REVIEWED_PACKAGE_EXAMPLES.get(boundary.package_name)
    if expected_examples is None:
        raise BoundaryError(
            f"{boundary.package_name} has no reviewed public-example inventory"
        )
    actual_examples = {
        entry
        for entry in entries
        if pathlib.PurePosixPath(entry).parts[:1] == ("examples",)
    }
    if actual_examples != expected_examples:
        raise BoundaryError(
            f"{boundary.package_name} public examples are not exactly artifact-qualified: "
            f"expected {sorted(expected_examples)!r}, "
            f"observed {sorted(actual_examples)!r}"
        )

    package_root = boundary.package_root.resolve()
    busl_hashes = _busl_source_hashes(boundary)
    for entry in entries:
        relative = _validate_safe_relative_path(entry, boundary.package_name)
        if entry in GENERATED_PACKAGE_ENTRIES:
            continue
        _reject_symlink_components(package_root, relative, boundary.package_name)
        candidate = package_root.joinpath(*relative.parts)
        if not candidate.is_file():
            raise BoundaryError(
                f"{boundary.package_name} package entry is not a regular source file: "
                f"{entry}"
            )
        try:
            candidate.resolve().relative_to(package_root)
        except ValueError as error:
            raise BoundaryError(
                f"{boundary.package_name} package entry escaped its Apache source root: "
                f"{entry}"
            ) from error
        try:
            encoded = candidate.read_bytes()
        except OSError as error:
            raise BoundaryError(
                f"{boundary.package_name} package source is unreadable: {entry}"
            ) from error
        _validate_public_source_bytes(
            boundary,
            entry,
            encoded,
            busl_hashes,
        )

    return tuple(entries)


def validate_deterministic_listings(
    package_name: str,
    first: str,
    second: str,
) -> None:
    """Require byte-identical package lists from identical Cargo invocations."""

    if first != second:
        raise BoundaryError(
            f"{package_name} package listing changed between identical Cargo invocations"
        )


def validate_staged_archive(
    boundary: Boundary,
    archive: pathlib.Path,
    expected_entries: Sequence[str],
) -> bytes:
    """Require an assembled `.crate` to contain exactly the reviewed file list."""

    if archive.is_symlink() or not archive.is_file():
        raise BoundaryError(
            f"{boundary.package_name} staged package archive is not a regular file"
        )
    suffix = ".crate"
    archive_name = archive.name
    expected_prefix = f"{boundary.package_name}-"
    if not archive_name.startswith(expected_prefix) or not archive_name.endswith(suffix):
        raise BoundaryError(
            f"{boundary.package_name} staged package archive has an unexpected name: "
            f"{archive_name}"
        )
    package_prefix = archive_name[: -len(suffix)]
    busl_hashes = _busl_source_hashes(boundary)

    try:
        with tarfile.open(archive, mode="r:*") as crate:
            entries = []
            for member in crate.getmembers():
                if not member.isfile():
                    raise BoundaryError(
                        f"{boundary.package_name} archive contains a non-file member: "
                        f"{member.name}"
                    )
                member_path = pathlib.PurePosixPath(member.name)
                if (
                    len(member_path.parts) < 2
                    or member_path.parts[0] != package_prefix
                ):
                    raise BoundaryError(
                        f"{boundary.package_name} archive member escaped its crate "
                        f"prefix: {member.name}"
                    )
                relative = pathlib.PurePosixPath(*member_path.parts[1:]).as_posix()
                entries.append(relative)
                extracted = crate.extractfile(member)
                if extracted is None:
                    raise BoundaryError(
                        f"{boundary.package_name} archive member is unreadable: "
                        f"{member.name}"
                    )
                _validate_public_source_bytes(
                    boundary,
                    relative,
                    extracted.read(),
                    busl_hashes,
                )
    except (tarfile.TarError, OSError) as error:
        raise BoundaryError(
            f"{boundary.package_name} staged package archive is unreadable: {error}"
        ) from error

    if tuple(entries) != tuple(expected_entries):
        raise BoundaryError(
            f"{boundary.package_name} staged archive does not match its cargo "
            "package listing"
        )
    return archive.read_bytes()


def _find_staged_archive(
    target_directory: pathlib.Path,
    package_name: str,
) -> pathlib.Path:
    package_directory = target_directory / "package"
    candidates = sorted(package_directory.glob(f"{package_name}-*.crate"))
    if len(candidates) != 1:
        raise BoundaryError(
            f"expected exactly one staged archive for {package_name}, "
            f"found {len(candidates)}"
        )
    return candidates[0]


def _run_command(
    command: Sequence[str],
    repo_root: pathlib.Path,
    environment: Mapping[str, str],
) -> str:
    try:
        completed = subprocess.run(
            command,
            cwd=repo_root,
            env=environment,
            check=True,
            capture_output=True,
            text=True,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    except FileNotFoundError as error:
        raise BoundaryError(f"required command is unavailable: {command[0]}") from error
    except subprocess.TimeoutExpired as error:
        raise BoundaryError(
            f"command exceeded {COMMAND_TIMEOUT_SECONDS}s timeout: {' '.join(command)}"
        ) from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "no diagnostic").strip()
        raise BoundaryError(
            f"command failed ({' '.join(command)}): {detail}"
        ) from error
    return completed.stdout


def verify(
    repo_root: pathlib.Path,
    command_runner: Callable[
        [Sequence[str], pathlib.Path, Mapping[str, str]], str
    ] = _run_command,
) -> dict[str, tuple[str, ...]]:
    """Run Cargo in a temporary staging root and verify both public boundaries."""

    resolved_root = repo_root.resolve()
    with tempfile.TemporaryDirectory(prefix="xenoteer-package-boundaries-") as staging:
        environment = dict(os.environ)
        environment.update(
            {
                "CARGO_TARGET_DIR": str(pathlib.Path(staging) / "target"),
                "CARGO_TERM_COLOR": "never",
                "LC_ALL": "C",
            }
        )
        metadata_command = (
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--all-features",
        )
        first_metadata = command_runner(
            metadata_command,
            resolved_root,
            environment,
        )
        second_metadata = command_runner(
            metadata_command,
            resolved_root,
            environment,
        )
        if first_metadata != second_metadata:
            raise BoundaryError(
                "cargo metadata changed between identical offline invocations"
            )
        closures = validate_dependency_closures(
            parse_metadata(first_metadata),
            resolved_root,
        )
        validate_registry_publish_metadata(
            parse_metadata(first_metadata),
            boundary_specs(resolved_root),
        )

        first_archives: dict[str, pathlib.Path] = {}
        for boundary in boundary_specs(resolved_root):
            list_target = pathlib.Path(staging) / "list-target"
            package_command = (
                "cargo",
                "package",
                "--locked",
                "--offline",
                "--allow-dirty",
                "--no-verify",
                "--exclude-lockfile",
                "--list",
                "--all-features",
                "--target-dir",
                str(list_target),
                "--package",
                boundary.package_name,
            )
            first_listing = command_runner(
                package_command,
                resolved_root,
                environment,
            )
            second_listing = command_runner(
                package_command,
                resolved_root,
                environment,
            )
            validate_deterministic_listings(
                boundary.package_name,
                first_listing,
                second_listing,
            )
            reviewed_entries = validate_package_listing(boundary, first_listing)

            archive_payloads = []
            for run in ("first", "second"):
                package_target = (
                    pathlib.Path(staging)
                    / f"{boundary.package_name}-{run}-target"
                )
                assemble_command = tuple(
                    argument
                    for argument in package_command
                    if argument != "--list"
                )
                target_index = assemble_command.index(str(list_target))
                assemble_command = (
                    *assemble_command[:target_index],
                    str(package_target),
                    *assemble_command[target_index + 1 :],
                )
                command_runner(
                    assemble_command,
                    resolved_root,
                    environment,
                )
                archive = _find_staged_archive(
                    package_target,
                    boundary.package_name,
                )
                if run == "first":
                    first_archives[boundary.package_name] = archive
                archive_payloads.append(
                    validate_staged_archive(
                        boundary,
                        archive,
                        reviewed_entries,
                    )
                )
                validate_packaged_manifest(
                    boundary.package_name,
                    read_archive_file(
                        archive,
                        boundary.package_name,
                        "Cargo.toml",
                    ),
                )
            if archive_payloads[0] != archive_payloads[1]:
                raise BoundaryError(
                    f"{boundary.package_name} archive bytes changed between "
                    "identical Cargo invocations"
                )

        resolution_root = pathlib.Path(staging) / "packaged-resolution"
        sdk_root = resolution_root / "xenoteer-sdk"
        protocol_root = resolution_root / "xenoteer-protocol"
        extract_validated_archive(
            first_archives["xenoteer-sdk"],
            "xenoteer-sdk",
            sdk_root,
        )
        extract_validated_archive(
            first_archives["xenoteer-protocol"],
            "xenoteer-protocol",
            protocol_root,
        )
        consumer_root = resolution_root / "consumer"
        (consumer_root / "src").mkdir(parents=True)
        (consumer_root / "src" / "lib.rs").write_text(
            "pub fn protocol_version() -> xenoteer_sdk::ProtocolVersion { "
            "xenoteer_sdk::ProtocolVersion::V1_0 }\n",
            encoding="utf-8",
        )
        (consumer_root / "Cargo.toml").write_text(
            "[package]\n"
            'name = "xenoteer-package-resolution-smoke"\n'
            'version = "0.0.0"\n'
            'edition = "2024"\n'
            "publish = false\n\n"
            "[dependencies]\n"
            f'xenoteer-sdk = {{ path = {json.dumps(str(sdk_root))} }}\n\n'
            "[patch.crates-io]\n"
            f'xenoteer-protocol = {{ path = {json.dumps(str(protocol_root))} }}\n\n'
            "[workspace]\n",
            encoding="utf-8",
        )
        resolution_output = command_runner(
            (
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--offline",
                "--manifest-path",
                str(consumer_root / "Cargo.toml"),
            ),
            consumer_root,
            environment,
        )
        validate_packaged_resolution_metadata(parse_metadata(resolution_output))
        command_runner(
            (
                "cargo",
                "check",
                "--offline",
                "--jobs",
                "2",
                "--manifest-path",
                str(consumer_root / "Cargo.toml"),
            ),
            consumer_root,
            environment,
        )

    return closures


def main() -> int:
    """CLI entry point."""

    repo_root = pathlib.Path(__file__).resolve().parents[2]
    try:
        closures = verify(repo_root)
    except BoundaryError as error:
        print(f"public package boundary verification failed: {error}", file=sys.stderr)
        return 1

    summaries = []
    for package_name in sorted(closures):
        local_packages = ",".join(closures[package_name])
        summaries.append(f"{package_name}=[{local_packages}]")
    print(
        "verified deterministic Apache-2.0 Cargo package boundaries: "
        + " ".join(summaries)
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
