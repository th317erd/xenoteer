# SPDX-License-Identifier: Apache-2.0
"""Focused failure-path tests for the public Cargo package boundary verifier."""

from __future__ import annotations

import importlib.util
import io
import json
import pathlib
import sys
import tarfile
import tempfile
import tomllib
import unittest


SCRIPT_PATH = pathlib.Path(__file__).resolve().parents[1] / "verify-boundaries.py"
SPEC = importlib.util.spec_from_file_location("verify_boundaries", SCRIPT_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load package-boundary verifier: {SCRIPT_PATH}")
verify_boundaries = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verify_boundaries
SPEC.loader.exec_module(verify_boundaries)


PROTOCOL_ID = "path+file:///workspace/crates/xenoteer-protocol#0.1.0"
SDK_ID = "path+file:///workspace/crates/xenoteer-sdk#0.1.0"
SERVER_ID = "path+file:///workspace/crates/xenoteer-server#0.1.0"
SERDE_ID = (
    "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.229"
)
REPOSITORY_ROOT = SCRIPT_PATH.parents[2]
CANONICAL_SDK_EXAMPLES = ("examples/phase6_behaviors.rs",)


def repository_published_sdk_examples() -> tuple[str, ...]:
    """Model the crate's literal example exclusions without invoking Cargo."""

    package_root = REPOSITORY_ROOT / "crates" / "xenoteer-sdk"
    manifest = tomllib.loads(
        (package_root / "Cargo.toml").read_text(encoding="utf-8")
    )
    package = manifest.get("package")
    if not isinstance(package, dict):
        raise AssertionError("xenoteer-sdk manifest omitted [package]")
    excluded = package.get("exclude", [])
    if not isinstance(excluded, list) or not all(
        isinstance(entry, str) for entry in excluded
    ):
        raise AssertionError("xenoteer-sdk package exclusions must be literal strings")
    candidates = (
        path.relative_to(package_root).as_posix()
        for path in (package_root / "examples").rglob("*")
        if path.is_file()
    )
    return tuple(sorted(candidate for candidate in candidates if candidate not in excluded))


def package(
    name: str,
    package_id: str,
    root: pathlib.Path,
    license_expression: str,
    source: str | None = None,
) -> dict[str, object]:
    """Build the relevant subset of one Cargo metadata package object."""

    return {
        "id": package_id,
        "name": name,
        "license": license_expression,
        "manifest_path": str(root / "crates" / name / "Cargo.toml"),
        "publish": None,
        "source": source,
    }


def node(package_id: str, *dependencies: str) -> dict[str, object]:
    """Build the relevant subset of one Cargo metadata resolve node."""

    return {
        "id": package_id,
        "deps": [{"name": dependency, "pkg": dependency} for dependency in dependencies],
    }


class PackageBoundaryTests(unittest.TestCase):
    """Prove success and fail-closed behavior without invoking Cargo."""

    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.root = pathlib.Path(self.temporary_directory.name).resolve()
        for crate in ("xenoteer-protocol", "xenoteer-sdk", "xenoteer-server"):
            crate_root = self.root / "crates" / crate
            (crate_root / "src").mkdir(parents=True)
            license_expression = (
                "BUSL-1.1" if crate == "xenoteer-server" else "Apache-2.0"
            )
            (crate_root / "Cargo.toml").write_text(
                f'[package]\nname = "{crate}"\n'
                f'version = "0.1.0"\nlicense = "{license_expression}"\n',
                encoding="utf-8",
            )
            source = (
                "// private server source\n"
                if crate == "xenoteer-server"
                else f"// public {crate} source\n"
            )
            (crate_root / "src" / "lib.rs").write_text(source, encoding="utf-8")
            (crate_root / "LICENSE").write_text("license\n", encoding="utf-8")
            (crate_root / "NOTICE").write_text("notice\n", encoding="utf-8")

    def valid_metadata(self) -> dict[str, object]:
        """Return a complete clean public-package dependency graph."""

        return {
            "workspace_members": [PROTOCOL_ID, SDK_ID, SERVER_ID],
            "packages": [
                package(
                    "xenoteer-protocol",
                    PROTOCOL_ID,
                    self.root,
                    "Apache-2.0",
                ),
                package("xenoteer-sdk", SDK_ID, self.root, "Apache-2.0"),
                package(
                    "xenoteer-server",
                    SERVER_ID,
                    self.root,
                    "BUSL-1.1",
                ),
                {
                    "id": SERDE_ID,
                    "name": "serde",
                    "license": "MIT OR Apache-2.0",
                    "manifest_path": "/cargo/registry/serde/Cargo.toml",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                },
            ],
            "resolve": {
                "nodes": [
                    node(PROTOCOL_ID, SERDE_ID),
                    node(SDK_ID, PROTOCOL_ID, SERDE_ID),
                    node(SERVER_ID, PROTOCOL_ID),
                    node(SERDE_ID),
                ]
            },
        }

    def test_clean_dependency_graph_and_package_listing_pass(self) -> None:
        metadata = self.valid_metadata()
        closures = verify_boundaries.validate_dependency_closures(
            metadata,
            self.root,
        )
        self.assertEqual(
            closures,
            {
                "xenoteer-protocol": ("xenoteer-protocol",),
                "xenoteer-sdk": ("xenoteer-protocol", "xenoteer-sdk"),
            },
        )

        listing = "\n".join(
            (
                ".cargo_vcs_info.json",
                "Cargo.lock",
                "Cargo.toml",
                "Cargo.toml.orig",
                "LICENSE",
                "NOTICE",
                "src/lib.rs",
                "",
            )
        )
        boundary = verify_boundaries.boundary_specs(self.root)[0]
        verify_boundaries.validate_package_listing(boundary, listing)
        verify_boundaries.validate_deterministic_listings(
            boundary.package_name,
            listing,
            listing,
        )
        verify_boundaries.validate_registry_publish_metadata(
            metadata,
            verify_boundaries.boundary_specs(self.root),
        )

    def test_published_sdk_has_one_artifact_qualified_example(self) -> None:
        self.assertEqual(
            repository_published_sdk_examples(),
            CANONICAL_SDK_EXAMPLES,
            "every public Cargo example must be qualified by the staged artifact gate",
        )

    def test_package_listing_rejects_an_unqualified_public_example(self) -> None:
        sdk = verify_boundaries.boundary_specs(self.root)[1]
        examples = sdk.package_root / "examples"
        examples.mkdir()
        canonical = examples / "phase6_behaviors.rs"
        canonical.write_text(
            "// SPDX-License-Identifier: Apache-2.0\nfn main() {}\n",
            encoding="utf-8",
        )
        listing = "\n".join(
            sorted(
                (
                    ".cargo_vcs_info.json",
                    "Cargo.lock",
                    "Cargo.toml",
                    "Cargo.toml.orig",
                    "LICENSE",
                    "NOTICE",
                    "examples/phase6_behaviors.rs",
                    "src/lib.rs",
                )
            )
        )
        verify_boundaries.validate_package_listing(sdk, listing)

        unqualified = examples / "legacy.rs"
        unqualified.write_text(
            "// SPDX-License-Identifier: Apache-2.0\nfn main() {}\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "public examples are not exactly artifact-qualified",
        ):
            verify_boundaries.validate_package_listing(
                sdk,
                "\n".join(sorted((*listing.splitlines(), "examples/legacy.rs"))),
            )

    def test_public_protocol_must_be_registry_publishable(self) -> None:
        metadata = self.valid_metadata()
        protocol = next(
            item for item in metadata["packages"] if item["id"] == PROTOCOL_ID
        )
        protocol["publish"] = []

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "xenoteer-protocol.*publish",
        ):
            verify_boundaries.validate_registry_publish_metadata(
                metadata,
                verify_boundaries.boundary_specs(self.root),
            )

    def test_normalized_sdk_manifest_must_resolve_protocol_from_registry(self) -> None:
        valid = b"""
[package]
name = "xenoteer-sdk"
version = "0.1.0"

[dependencies.xenoteer-protocol]
version = "=0.1.0"
"""
        verify_boundaries.validate_packaged_manifest("xenoteer-sdk", valid)

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "registry-resolvable",
        ):
            verify_boundaries.validate_packaged_manifest(
                "xenoteer-sdk",
                valid.replace(
                    b'version = "=0.1.0"',
                    b'version = "=0.1.0"\npath = "../xenoteer-protocol"',
                ),
            )

    def test_packaged_resolution_requires_an_exact_registry_dependency(self) -> None:
        metadata = {
            "packages": [
                {
                    "name": "xenoteer-sdk",
                    "dependencies": [
                        {
                            "name": "xenoteer-protocol",
                            "req": "=0.1.0",
                            "path": None,
                            "source": "registry+https://github.com/rust-lang/crates.io-index",
                        }
                    ],
                },
                {"name": "xenoteer-protocol", "dependencies": []},
            ]
        }
        verify_boundaries.validate_packaged_resolution_metadata(metadata)
        metadata["packages"][0]["dependencies"][0]["path"] = "../protocol"
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "packaged SDK.*registry",
        ):
            verify_boundaries.validate_packaged_resolution_metadata(metadata)

    def test_bsl_workspace_dependency_is_rejected(self) -> None:
        metadata = self.valid_metadata()
        resolve_nodes = metadata["resolve"]["nodes"]
        resolve_nodes[1] = node(SDK_ID, PROTOCOL_ID, SERVER_ID, SERDE_ID)

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "BUSL-1.1.*xenoteer-server",
        ):
            verify_boundaries.validate_dependency_closures(metadata, self.root)

    def test_bsl_dependency_is_rejected_even_if_it_claims_remote_source(self) -> None:
        metadata = self.valid_metadata()
        server = next(
            item for item in metadata["packages"] if item["id"] == SERVER_ID
        )
        server["source"] = "git+https://example.invalid/xenoteer-server"
        metadata["workspace_members"].remove(SERVER_ID)
        resolve_nodes = metadata["resolve"]["nodes"]
        resolve_nodes[1] = node(SDK_ID, PROTOCOL_ID, SERVER_ID, SERDE_ID)

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "BUSL-1.1.*xenoteer-server",
        ):
            verify_boundaries.validate_dependency_closures(metadata, self.root)

    def test_unapproved_apache_workspace_dependency_is_rejected(self) -> None:
        metadata = self.valid_metadata()
        server = next(
            item for item in metadata["packages"] if item["id"] == SERVER_ID
        )
        server["license"] = "Apache-2.0"
        resolve_nodes = metadata["resolve"]["nodes"]
        resolve_nodes[1] = node(SDK_ID, PROTOCOL_ID, SERVER_ID, SERDE_ID)

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "unexpected local package xenoteer-server",
        ):
            verify_boundaries.validate_dependency_closures(metadata, self.root)

    def test_missing_notice_and_path_escape_are_rejected(self) -> None:
        boundary = verify_boundaries.boundary_specs(self.root)[0]
        without_notice = "\n".join(
            ("Cargo.toml", "LICENSE", "src/lib.rs", "")
        )
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "missing required package entries: NOTICE",
        ):
            verify_boundaries.validate_package_listing(boundary, without_notice)

        escaped = "\n".join(
            ("../xenoteer-server/src/lib.rs", "Cargo.toml", "LICENSE", "NOTICE", "")
        )
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "unsafe package path",
        ):
            verify_boundaries.validate_package_listing(boundary, escaped)

    def test_symlinked_source_is_rejected(self) -> None:
        boundary = verify_boundaries.boundary_specs(self.root)[0]
        source = boundary.package_root / "src" / "lib.rs"
        source.unlink()
        source.symlink_to(self.root / "crates" / "xenoteer-server" / "src" / "lib.rs")
        listing = "\n".join(
            ("Cargo.toml", "LICENSE", "NOTICE", "src/lib.rs", "")
        )

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "symlinked package source",
        ):
            verify_boundaries.validate_package_listing(boundary, listing)

    def test_copied_bsl_source_and_non_apache_marker_are_rejected(self) -> None:
        boundary = verify_boundaries.boundary_specs(self.root)[1]
        source = boundary.package_root / "src" / "lib.rs"
        example = boundary.package_root / "examples" / "phase6_behaviors.rs"
        example.parent.mkdir()
        example.write_text(
            "// SPDX-License-Identifier: Apache-2.0\nfn main() {}\n",
            encoding="utf-8",
        )
        listing = "\n".join(
            (
                "Cargo.toml",
                "LICENSE",
                "NOTICE",
                "examples/phase6_behaviors.rs",
                "src/lib.rs",
                "",
            )
        )

        private_source = (
            self.root / "crates" / "xenoteer-server" / "src" / "lib.rs"
        ).read_bytes()
        source.write_bytes(private_source)
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "copied BUSL-1.1 source",
        ):
            verify_boundaries.validate_package_listing(boundary, listing)

        source.write_text(
            "// SPDX-License-Identifier: BUSL-1.1\npub fn copied() {}\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "non-Apache source license marker",
        ):
            verify_boundaries.validate_package_listing(boundary, listing)

    def test_nondeterministic_or_malformed_cargo_output_is_rejected(self) -> None:
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "changed between identical Cargo invocations",
        ):
            verify_boundaries.validate_deterministic_listings(
                "xenoteer-sdk",
                "Cargo.toml\nLICENSE\nNOTICE\n",
                "Cargo.toml\nLICENSE\nNOTICE\nsrc/lib.rs\n",
            )

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "invalid JSON",
        ):
            verify_boundaries.parse_metadata("{not json")

        metadata = self.valid_metadata()
        metadata["resolve"] = None
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "resolve graph",
        ):
            verify_boundaries.validate_dependency_closures(metadata, self.root)

    def test_staged_archive_must_exactly_match_the_reviewed_listing(self) -> None:
        boundary = verify_boundaries.boundary_specs(self.root)[0]
        archive = self.root / "xenoteer-protocol-0.1.0.crate"
        prefix = "xenoteer-protocol-0.1.0"
        with tarfile.open(archive, "w:gz") as crate:
            for name in ("Cargo.toml", "LICENSE", "NOTICE", "src/lib.rs"):
                payload = name.encode("utf-8")
                member = tarfile.TarInfo(f"{prefix}/{name}")
                member.size = len(payload)
                crate.addfile(member, io.BytesIO(payload))

        archive_bytes = verify_boundaries.validate_staged_archive(
            boundary,
            archive,
            ("Cargo.toml", "LICENSE", "NOTICE", "src/lib.rs"),
        )
        self.assertEqual(archive_bytes, archive.read_bytes())

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "does not match its cargo package listing",
        ):
            verify_boundaries.validate_staged_archive(
                boundary,
                archive,
                ("Cargo.toml", "LICENSE", "NOTICE"),
            )

    def test_staged_archive_rejects_late_bsl_source_substitution(self) -> None:
        boundary = verify_boundaries.boundary_specs(self.root)[0]
        archive = self.root / "xenoteer-protocol-0.1.0.crate"
        prefix = "xenoteer-protocol-0.1.0"
        entries = ("Cargo.toml", "LICENSE", "NOTICE", "src/lib.rs")
        with tarfile.open(archive, "w:gz") as crate:
            for name in entries:
                payload = (
                    b"// SPDX-License-Identifier: BUSL-1.1\n"
                    if name == "src/lib.rs"
                    else name.encode("utf-8")
                )
                member = tarfile.TarInfo(f"{prefix}/{name}")
                member.size = len(payload)
                crate.addfile(member, io.BytesIO(payload))

        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "non-Apache source license marker",
        ):
            verify_boundaries.validate_staged_archive(
                boundary,
                archive,
                entries,
            )

        private_source = (
            self.root / "crates" / "xenoteer-server" / "src" / "lib.rs"
        ).read_bytes()
        with tarfile.open(archive, "w:gz") as crate:
            for name in entries:
                payload = (
                    private_source
                    if name == "src/lib.rs"
                    else name.encode("utf-8")
                )
                member = tarfile.TarInfo(f"{prefix}/{name}")
                member.size = len(payload)
                crate.addfile(member, io.BytesIO(payload))
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "copied BUSL-1.1 source",
        ):
            verify_boundaries.validate_staged_archive(
                boundary,
                archive,
                entries,
            )

    def test_metadata_parser_requires_an_object(self) -> None:
        with self.assertRaisesRegex(
            verify_boundaries.BoundaryError,
            "top-level object",
        ):
            verify_boundaries.parse_metadata(json.dumps([]))


if __name__ == "__main__":
    unittest.main()
