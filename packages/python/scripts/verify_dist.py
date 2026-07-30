#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify exact reviewed wheel and sdist file boundaries."""

from __future__ import annotations

import argparse
import re
import stat
import tarfile
import tomllib
from pathlib import Path
from zipfile import ZipFile


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
_PROJECT = tomllib.loads(
    (PACKAGE_ROOT / "pyproject.toml").read_text(encoding="utf-8")
)
_PROJECT_VERSION = _PROJECT["project"]["version"]
_APPROVED_WHEEL_DIST_INFO = f"xenoteer-{_PROJECT_VERSION}.dist-info"
_APPROVED_SDIST_ROOT = f"xenoteer-{_PROJECT_VERSION}"
_SPDX_MARKER = re.compile(
    r"SPDX\s*-\s*License\s*-\s*Identifier\s*:\s*[^\r\n]*",
    re.IGNORECASE,
)
_APACHE_IDENTIFIER = "SP" + "DX-License-Identifier: Apache-2.0"
_APACHE_MARKER = "# " + _APACHE_IDENTIFIER


def _allowlist(name: str) -> set[str]:
    return {
        line
        for line in (PACKAGE_ROOT / name).read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    }


def verify_wheel(path: Path) -> int:
    expected = _allowlist("WHEEL_ALLOWLIST.txt")
    with ZipFile(path) as archive:
        members = archive.infolist()
        _verify_archive_names(
            [member.filename for member in members],
            "wheel",
        )
        logical_names: list[str] = []
        dist_info_identities: set[str] = set()
        for member in members:
            logical, dist_info_identity = _wheel_logical_name(member.filename)
            logical_names.append(logical)
            if dist_info_identity is not None:
                dist_info_identities.add(dist_info_identity)
        if dist_info_identities != {_APPROVED_WHEEL_DIST_INFO}:
            raise RuntimeError(
                "wheel dist-info identity mismatch: "
                f"expected={_APPROVED_WHEEL_DIST_INFO!r} "
                f"actual={sorted(dist_info_identities)!r}"
            )
        _verify_logical_name_uniqueness(logical_names, "wheel")
        for member in members:
            mode = (member.external_attr >> 16) & 0o177777
            if stat.S_ISLNK(mode):
                raise RuntimeError(f"wheel contains symbolic link {member.filename!r}")
        raw = {name for name in archive.namelist() if not name.endswith("/")}
        actual = {_wheel_logical_name(name)[0] for name in raw}
        if actual != expected:
            raise RuntimeError(
                f"wheel boundary mismatch: extra={sorted(actual - expected)!r} "
                f"missing={sorted(expected - actual)!r}"
            )
        metadata_name = next(
            name for name in raw if name.endswith(".dist-info/METADATA")
        )
        metadata = archive.read(metadata_name).decode("utf-8")
        for required in (
            "License-Expression: Apache-2.0",
            "Requires-Dist: httpx",
            "Requires-Dist: websockets",
        ):
            if required not in metadata:
                raise RuntimeError(f"wheel metadata is missing {required!r}")
        if any("xenoteerd" in name or "/server" in name for name in raw):
            raise RuntimeError("wheel contains server implementation")
        _verify_python_sources(
            {
                name: archive.read(name)
                for name in raw
                if name.endswith(".py")
            }
        )
    return len(actual)


def verify_sdist(path: Path) -> int:
    expected = _allowlist("SDIST_ALLOWLIST.txt")
    with tarfile.open(path) as archive:
        members = archive.getmembers()
        _verify_archive_names(
            [member.name for member in members],
            "sdist",
        )
        roots = {
            member.name.removesuffix("/").split("/", 1)[0]
            for member in members
        }
        if roots != {_APPROVED_SDIST_ROOT}:
            raise RuntimeError(
                "sdist root identity mismatch: "
                f"expected={_APPROVED_SDIST_ROOT!r} actual={sorted(roots)!r}"
            )
        _verify_logical_name_uniqueness(
            [_sdist_logical_name(member.name) for member in members],
            "sdist",
        )
        forbidden_types = [
            member.name
            for member in members
            if not (member.isfile() or member.isdir())
        ]
        if forbidden_types:
            raise RuntimeError(
                f"sdist contains non-regular members: {sorted(forbidden_types)!r}"
            )
        files = [member.name for member in members if member.isfile()]
        actual: set[str] = set()
        for name in files:
            actual.add(_sdist_logical_name(name))
        if actual != expected:
            raise RuntimeError(
                f"sdist boundary mismatch: extra={sorted(actual - expected)!r} "
                f"missing={sorted(expected - actual)!r}"
            )
        if any("xenoteerd" in name or "/crates/" in name for name in files):
            raise RuntimeError("sdist contains server implementation")
        sources: dict[str, bytes] = {}
        for member in members:
            if member.isfile() and member.name.endswith(".py"):
                extracted = archive.extractfile(member)
                if extracted is None:
                    raise RuntimeError(f"could not read {member.name!r}")
                sources[member.name] = extracted.read()
        _verify_python_sources(sources)
    return len(actual)


def _verify_archive_names(names: list[str], kind: str) -> None:
    seen: set[str] = set()
    for name in names:
        canonical = name[:-1] if name.endswith("/") else name
        components = canonical.split("/")
        if (
            not canonical
            or canonical.startswith("/")
            or "\\" in canonical
            or any(component in {"", ".", ".."} for component in components)
            or any(ord(character) < 0x20 for character in canonical)
        ):
            raise RuntimeError(f"{kind} contains unsafe member name {name!r}")
        if canonical in seen:
            raise RuntimeError(
                f"{kind} contains duplicate member name {name!r}"
            )
        seen.add(canonical)


def _verify_logical_name_uniqueness(names: list[str], kind: str) -> None:
    seen: set[str] = set()
    for name in names:
        if name in seen:
            raise RuntimeError(
                f"{kind} contains duplicate normalized member name {name!r}"
            )
        seen.add(name)


def _wheel_logical_name(name: str) -> tuple[str, str | None]:
    canonical = name.removesuffix("/")
    components = canonical.split("/")
    identity = components[0]
    if not identity.endswith(".dist-info"):
        return canonical, None
    logical = "/".join(("DIST_INFO", *components[1:]))
    return logical, identity


def _sdist_logical_name(name: str) -> str:
    canonical = name.removesuffix("/")
    components = canonical.split("/")
    relative = "/".join(components[1:])
    if relative.startswith("src/xenoteer.egg-info/"):
        return relative.replace(
            "src/xenoteer.egg-info/",
            "EGG_INFO/",
            1,
        )
    return relative


def _verify_python_sources(sources: dict[str, bytes]) -> None:
    for name, encoded in sources.items():
        try:
            text = encoded.decode("utf-8")
        except UnicodeDecodeError:
            raise RuntimeError(f"Python source {name!r} is not UTF-8") from None
        first_lines = text.splitlines()[:3]
        markers = _SPDX_MARKER.findall(text)
        if (
            first_lines.count(_APACHE_MARKER) != 1
            or markers != [_APACHE_IDENTIFIER]
        ):
            raise RuntimeError(
                f"Python source {name!r} must contain exactly one Apache SPDX marker"
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("wheel", type=Path)
    parser.add_argument("sdist", type=Path)
    arguments = parser.parse_args()
    wheel_count = verify_wheel(arguments.wheel)
    sdist_count = verify_sdist(arguments.sdist)
    print(
        f"distribution boundaries verified: "
        f"wheel={wheel_count} sdist={sdist_count}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
