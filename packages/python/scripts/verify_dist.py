#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify exact reviewed wheel and sdist file boundaries."""

from __future__ import annotations

import argparse
import re
import stat
import tarfile
from pathlib import Path
from zipfile import ZipFile


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
_DIST_INFO = re.compile(r"xenoteer-[^/]+\.dist-info")


def _allowlist(name: str) -> set[str]:
    return {
        line
        for line in (PACKAGE_ROOT / name).read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    }


def verify_wheel(path: Path) -> int:
    expected = _allowlist("WHEEL_ALLOWLIST.txt")
    with ZipFile(path) as archive:
        for member in archive.infolist():
            mode = (member.external_attr >> 16) & 0o177777
            if stat.S_ISLNK(mode):
                raise RuntimeError(f"wheel contains symbolic link {member.filename!r}")
        raw = {name for name in archive.namelist() if not name.endswith("/")}
        actual = {_DIST_INFO.sub("DIST_INFO", name) for name in raw}
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
            relative = name.split("/", 1)[1]
            relative = relative.replace("src/xenoteer.egg-info/", "EGG_INFO/")
            actual.add(relative)
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


def _verify_python_sources(sources: dict[str, bytes]) -> None:
    for name, encoded in sources.items():
        try:
            text = encoded.decode("utf-8")
        except UnicodeDecodeError:
            raise RuntimeError(f"Python source {name!r} is not UTF-8") from None
        first_lines = text.splitlines()[:3]
        if "# SPDX-License-Identifier: Apache-2.0" not in first_lines:
            raise RuntimeError(f"Python source {name!r} lacks Apache SPDX provenance")
        lowered = text.lower()
        forbidden_marker = "spdx-license-identifier:" + " busl-1.1"
        if forbidden_marker in lowered:
            raise RuntimeError(f"Python source {name!r} contains BSL implementation")


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
