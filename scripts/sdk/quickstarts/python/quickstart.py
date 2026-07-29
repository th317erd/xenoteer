#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Minimal installed-package Xenoteer status quick-start."""

from __future__ import annotations

import asyncio
import os
import re
import sys
from pathlib import Path

import xenoteer
from xenoteer import ClientOptions, XenoteerClient, XenoteerError


def required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"required environment is missing: {name}")
    return value


def verify_installed_origin() -> None:
    expected = Path(required("XENOTEER_EXPECTED_INSTALL_ROOT")).resolve(strict=True)
    module_file = Path(xenoteer.__file__).resolve(strict=True)
    if not module_file.is_relative_to(expected):
        raise RuntimeError("Python SDK resolved outside the staged distribution installation")


async def exercise() -> None:
    verify_installed_origin()
    language = required("XENOTEER_QUICKSTART_LANGUAGE")
    if re.fullmatch(r"[a-z-]+", language) is None:
        raise RuntimeError("quick-start language label is invalid")
    expect_authentication_failure = required("XENOTEER_EXPECT_AUTH_FAILURE") == "1"
    options = ClientOptions(
        base_url=required("XENOTEER_API_BASE"),
        token=required("XENOTEER_TOKEN"),
        request_timeout=5,
    )
    try:
        client = await XenoteerClient.connect(options)
    except XenoteerError as error:
        if (
            expect_authentication_failure
            and error.code == "authentication"
            and error.status == 401
        ):
            print(f"quickstart-ok language={language} mode=auth-failure")
            return
        raise

    async with client:
        if expect_authentication_failure:
            raise RuntimeError("invalid bearer unexpectedly authenticated")
        if (
            client.negotiated_protocol.major != 1
            or client.negotiated_protocol.minor != 0
        ):
            raise RuntimeError("server did not negotiate frozen protocol v1.0")
        if client.status.desktop.state != "ready":
            raise RuntimeError("desktop was not ready")
        client.desktop()
        print(f"quickstart-ok language={language} mode=success")


def main() -> int:
    try:
        asyncio.run(asyncio.wait_for(exercise(), timeout=8))
    except XenoteerError as error:
        print(
            f"public Python quick-start failed: XenoteerError[{error.code}]",
            file=sys.stderr,
        )
        return 1
    except (OSError, RuntimeError, asyncio.TimeoutError) as error:
        print(f"public Python quick-start failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
