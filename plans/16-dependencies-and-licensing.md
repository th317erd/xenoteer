# Dependencies, Licensing, and Source Fulfillment

Status: normative plan for dependency admission, license boundaries, notices,
corresponding source, and release evidence.

This document is an engineering compliance plan, not legal advice. Counsel should
review the first commercial release and any proposed dependency whose copyleft
boundary is not already covered here. The implementation team is nevertheless
expected to make the artifacts below mechanically reproducible; "legal will sort
it out later" is not an acceptable release strategy.

## 1. Decisions

1. Original Xenoteer server/runtime code is source-available under the repository
   `LICENSE`, a Business Source License 1.1 grant with Xenoteer's Additional Use
   Grant.
2. Each Xenoteer version changes to Apache License 2.0 no later than four years
   after its first public distribution. The current license also names July 20,
   2030 as the Change Date for version 0.1.0. On that transition, the managed-
   service and competing-offering restrictions for that version end.
3. Public protocol schemas and separately distributed client SDKs use Apache-2.0
   so users can integrate without importing the server's BSL terms into their
   applications.
4. Third-party programs, libraries, fonts, themes, and assets retain their own
   licenses. They are not relicensed as Xenoteer and are explicitly excluded from
   the Licensed Work by the root `LICENSE`.
5. GPL tools such as x11vnc remain separate executables connected through normal
   operating-system interfaces. Original BSL code does not link to GPL libraries
   without a specific compatibility and derivative-work review.
6. Every released image has a machine-readable package/source manifest, SBOM,
   human-readable notices, full license texts, and a durable corresponding-source
   fulfillment artifact tied to the image digest.
7. License evidence is generated from the exact final image and locked source
   graph, never copied from an earlier release or inferred from the Dockerfile.

## 2. What is being distributed

The release is an aggregate containing several legally distinct sets of work:

| Distribution unit | Expected terms | Boundary and packaging rule |
|---|---|---|
| `xenoteerd`, first-party X11/AT-SPI/runtime crates, first-party scripts and server docs | Repository BSL 1.1 parameters | Root `LICENSE`; SPDX identifier `BUSL-1.1` plus a copyright header in substantial source files |
| Protocol schema and generated-model source | Apache-2.0 | A self-contained `LICENSE` and `NOTICE` in its package/directory; no BSL-only implementation copied into it |
| Rust, TypeScript, and Python client SDKs | Apache-2.0 | Each published package includes Apache license metadata and text; package source remains usable without server source |
| Debian base and installed Debian packages | Package-specific terms | Preserve Debian copyright data and license texts; publish binary-to-source mapping and corresponding source |
| noVNC | MPL-2.0 | Keep notices; make source for the shipped version and any modified MPL-covered files available |
| websockify | LGPL-3.0 (verify at lock/update time) | Run as a separate Python program; retain notices/source and comply with the exact shipped version's terms |
| x11vnc | GPL-2.0 (verify exact repository/package expression) | Run as a separate executable; publish exact corresponding source, patches, and build/install provenance |
| Fonts, icons, themes, fixtures, and test media | Asset-specific terms | Admit only reviewed redistributable assets; record authorship/source/license per asset |

The container image as a transport is not assigned one blanket license. User-facing
materials say which first-party components are BSL and point to the third-party
inventory. They must not say that Debian, XFCE, Xorg, noVNC, or other packaged works
are "licensed under the Xenoteer license."

## 3. First-party license layout

The intended repository layout is:

```text
LICENSE                         # BSL parameters and standard text
NOTICE                          # first-party notice + map to third-party data
crates/xenoteerd/               # BSL
crates/xenoteer-core/           # BSL unless moved to public protocol package
crates/xenoteer-x11/            # BSL
crates/xenoteer-atspi/          # BSL
crates/xenoteer-server/         # BSL
crates/xenoteer-protocol/
  LICENSE                       # Apache-2.0
  NOTICE
sdks/rust/LICENSE               # Apache-2.0
sdks/typescript/LICENSE         # Apache-2.0
sdks/python/LICENSE             # Apache-2.0
third_party/
  README.md                     # provenance and modification index
dist/licenses/                  # generated; never the authoritative source
```

### 3.1 Protocol boundary

The Apache package may contain:

- request/response/event schemas;
- wire enums and validation limits that clients must share;
- generated data models and serializers;
- public API examples and compatibility fixtures;
- non-secret conformance tests.

It must not contain the coordinator, input algorithms, X11 implementation,
selection ownership engine, AT-SPI cache implementation, security enforcement, or
other server mechanics. Those stay in BSL crates. The server may depend on the
Apache protocol crate; the inverse dependency is forbidden.

Generated SDK files carry a generator notice and point to Apache-2.0. Templates and
the generator are licensed according to their own directory; generated outputs do
not silently inherit a root license that contradicts package metadata.

### 3.2 Version-specific BSL administration

BSL applies separately to each released version. Release automation therefore
requires an explicit license-parameter review when the public version changes:

1. Update `Licensed Work` to identify the released version or an intentionally
   defined version family.
2. Record the version's first-public-distribution timestamp.
3. Ensure its Change Date is no more than four years later.
4. Keep Apache-2.0 as the Change License unless the project owner makes an explicit,
   reviewed change compatible with the BSL covenant.
5. Store the effective parameters with the signed release manifest so historical
   versions remain unambiguous.

Do not retroactively move a published version's Change Date later. When a version
reaches its change date, tag/archive the Apache-2.0 source state or otherwise make
the transition easy for a recipient to prove. Marketing calls the pre-change code
"source-available," not "open source."

## 4. Dependency admission policy

Every new dependency is reviewed before merge. Prefer, in order:

1. a Rust or system component already in Debian stable and actively maintained;
2. a focused library with a small, auditable transitive graph;
3. a separate executable with a stable protocol when it avoids an incompatible
   link-time license or fragile native ABI;
4. a small first-party implementation only when protocol complexity and security
   risk are lower than adopting the dependency.

The review records:

- exact upstream project and canonical source URL;
- package/crate/module name and locked version or commit;
- why it is required and alternatives considered;
- SPDX license expression verified from the shipped source, including exceptions;
- link/IPC/asset/build-only relationship to first-party code;
- native code, `unsafe`, build scripts, proc macros, network behavior, and runtime
  privileges;
- maintenance activity, security policy, advisory history, and replacement cost;
- Debian source package mapping when installed by APT;
- multi-architecture availability and reproducibility constraints.

Unknown, custom, non-commercial, no-derivatives, source-absent, or contradictory
license metadata blocks admission. A repository badge or package-index label alone
is not proof: compare the source tree's license files, manifest expression, and
copyright notices.

## 5. Planned dependency boundaries

This table is a design guide, not a substitute for scanning the locked release.
License expressions must be reverified when versions change.

| Component family | How Xenoteer uses it | Boundary | Review focus |
|---|---|---|---|
| `x11rb` | X11 protocol, extensions, events, capture | Rust library linked into BSL process | Enable only required features; preserve MIT/Apache-2.0 notices; inspect generated protocol code terms |
| `xkbcommon` / Rust binding | key names, layouts, consumed modifiers | Dynamic native library and Rust binding | Library and binding have separate licenses; ABI/package availability; avoid copying tables without provenance |
| Tokio, Axum, Tower, Hyper | async runtime and API transport | Rust libraries linked into BSL process | Pin coherent versions; audit optional TLS/WS/HTTP transitive graph |
| Serde and schema tooling | wire/config models | Rust libraries; generators may affect Apache outputs | Generated-code notices and template license; deterministic schemas |
| `zbus` and `atspi` crate | session D-Bus and accessibility | Rust libraries linked into BSL process | Verify exact crate expressions and feature graph; D-Bus XML/spec is not runtime source code |
| `tracing`, metrics, error crates | observability/error plumbing | Rust libraries linked into BSL process | Redaction is a product issue; license remains normal permissive dependency review |
| Debian/Xorg/XFCE/AT-SPI packages | operating environment | Aggregated packages in image | GPL/LGPL/permissive mixture, copyright retention, source-package mapping, patches |
| x11vnc | optional VNC observation adapter | Separate supervised process over X11/socket | GPL source fulfillment; unmaintained upstream risk; never call library code from BSL process |
| websockify | VNC WebSocket bridge | Separate supervised Python process | LGPL compliance, Python dependency graph, proxy/auth deployment |
| noVNC | browser viewer assets | Static web application served/gated separately | MPL file-level obligations, asset/npm inventory, preserve source notices |
| `xdotool` | diagnostic compatibility tool only | Separate optional command | GPL executable/source obligations; never the authoritative automation path |

Process separation is an architectural risk-control boundary, not a magic legal
test. If first-party code and a copyleft program later exchange intimate,
implementation-specific internals or are packaged as one combined derivative work,
the change requires counsel review. Conversely, do not invent an IPC wrapper solely
to evade a license obligation.

### 5.1 Copyleft rules of thumb

- Permissive dependencies can normally be linked when their attribution/license
  requirements are preserved.
- MPL-2.0 is file-level: modifications to covered files must remain available under
  MPL terms. Keep first-party viewer customizations in separate files where the API
  supports it, and publish modified covered files.
- LGPL dynamic linking generally preserves replacement/relink rights and requires
  notices/source for the LGPL work; static linking and modifications need additional
  handling. Treat Python imports as a reviewed library relationship rather than
  assuming "it is a script" settles the question.
- GPL-covered standalone executables can be aggregated with differently licensed
  programs, but modifications and distribution still trigger GPL obligations. Do
  not link their GPL code into BSL binaries.
- License compatibility and source-availability duties are separate questions. A
  compatible relationship can still require notices, source, installation
  information, or disclosure of modifications.

## 6. Debian and container-image compliance

APT installs binaries, but Debian source packages and copyright files are the
compliance ground truth. The final-image build emits, for every installed package:

- binary package name, architecture, epoch/version/revision;
- source package name and source version (which can differ from the binary name);
- installed file inventory and relevant diversions/alternatives;
- `/usr/share/doc/<package>/copyright` digest and parsed license identifiers where
  practicable;
- Debian snapshot/repository URL and Release/InRelease digest;
- whether Xenoteer patched or rebuilt the package;
- corresponding-source archive/checksum and retrieval location.

Do not remove `/usr/share/doc/*/copyright`, license texts, or required notices in a
size-reduction pass. If documentation must be removed, the release assembler first
copies all legally required material into `/usr/share/licenses` and proves the
mapping. Image size is not a reason to make compliance depend on a website.

### 6.1 Source fulfillment

For every public image digest, produce a `xenoteer-sources-<version>.tar.zst` (or an
equivalent immutable OCI artifact) containing:

- exact Debian source packages for all installed GPL/LGPL and other source-required
  works, plus a conservative full-source set where automation cannot classify;
- upstream source archives/commits for vendored or manually installed programs;
- all Xenoteer modifications as applied source or patches;
- build scripts, package rules, and configuration needed by the applicable license;
- noVNC/websockify/x11vnc sources matching the shipped files exactly;
- a signed manifest mapping every binary/image path to source and license records.

Publish that artifact beside the image for at least the period required by the
applicable licenses and project retention policy. Do not rely solely on Debian,
GitHub, crates.io, npm, PyPI, or an upstream developer continuing to host it. A
written offer is used only with deliberate legal review; immediate source access is
the default.

If a recipient needs installation information under a license's anti-lockdown
provisions, the hardened image must not prevent them from replacing the covered
component in a way the license prohibits. Document rebuilding and replacing the
image component even though the container is otherwise immutable at runtime.

## 7. Generated release artifacts

Each final image contains:

```text
/usr/share/doc/xenoteer/LICENSE
/usr/share/doc/xenoteer/NOTICE
/usr/share/doc/xenoteer/THIRD_PARTY_NOTICES.html
/usr/share/doc/xenoteer/package-manifest.json
/usr/share/doc/xenoteer/source-manifest.json
/usr/share/licenses/<component>/...
```

The registry/release page additionally carries:

- SPDX and CycloneDX SBOMs tied to the image digest;
- the corresponding-source bundle and checksum;
- license/notices bundle;
- provenance attestation and signature described in
  [14-build-release-operations.md](14-build-release-operations.md);
- first-party source tag/archive and exact license parameters;
- SDK/schema packages with their independent Apache-2.0 metadata.

### 7.1 Source manifest minimum schema

Each record contains:

```json
{
  "component": "x11vnc",
  "binary_paths": ["/usr/bin/x11vnc"],
  "binary_version": "distribution-version",
  "source_name": "distribution-source-name",
  "source_version": "distribution-source-version",
  "source_sha256": "hex",
  "source_artifact_path": "sources/...",
  "license_expression": "reviewed SPDX expression",
  "license_evidence_paths": ["licenses/..."],
  "modified": false,
  "patch_paths": [],
  "relationship": "separate-process"
}
```

The actual version/hash/license values are generated from the lock and final image;
placeholders are forbidden in a release artifact. Files are sorted deterministically
so diffs expose dependency changes.

## 8. Automated enforcement

### 8.1 Rust graph

CI runs from the committed `Cargo.lock`:

- `cargo metadata --locked` for the complete feature-resolved graph;
- RustSec advisory scanning;
- `cargo deny check advisories bans licenses sources` with a reviewed allowlist;
- a notice generator such as `cargo-about`, using a committed template and accepted
  license inventory;
- duplicate/native/unknown-git-source reports for human review.

Allowlisting a license is not allowlisting every package with that license. Git
dependencies require a pinned full commit and source archive; wildcard Git branches
and unreviewed alternate registries fail. Any crate that declares multiple possible
licenses is resolved against actual included files and the project's chosen option.

### 8.2 OS, Python, and browser assets

The compliance job starts a container from the exact candidate digest and:

1. queries `dpkg` status/source metadata and hashes installed copyright files;
2. resolves Debian source packages from the pinned snapshot;
3. inventories Python distributions/modules used by websockify and helpers;
4. inventories npm/vendor assets used by noVNC or the gateway;
5. compares filesystem packages against an allowlisted, reviewed manifest;
6. scans for executable/vendor files not owned by a declared package;
7. builds notices and source bundles, then verifies every entry offline.

The offline verification unpacks the source bundle in a network-disabled job and
proves that every source-required manifest record resolves to a checksum-matching
file. This catches the common failure where an SBOM is present but the offered
source URL has disappeared.

### 8.3 CI failure policy

Release-blocking findings include:

- unknown/no-assertion/proprietary/non-redistributable dependency terms;
- a binary with no package/source provenance;
- missing license text, copyright notice, patch, or corresponding source;
- root BSL code accidentally packaged as Apache SDK code, or the reverse;
- a GPL library linked into a BSL executable without explicit approval;
- an SDK artifact that inherits the root BSL because its package lacks a license;
- SBOM, notices, source manifest, and final image disagreeing on versions;
- a license/advisory exception with no owner or expired review date.

## 9. Exceptions and dependency updates

An exception is a versioned repository file containing:

- dependency and exact version/digest;
- finding being waived and concrete impact;
- legal/security/engineering rationale;
- compensating control;
- named owner, reviewer, issue, and expiry date;
- removal/replacement plan.

Exceptions cannot waive an actual redistribution or corresponding-source
obligation. Automated update PRs must include dependency tree, license, notice,
source-package, image-size, architecture, and advisory diffs. A version bump that
changes license expression or upstream ownership never auto-merges.

Forks record their upstream base, all patches, rebasing method, and public source
location. If Xenoteer becomes the de facto maintainer of a viewer adapter or other
security-sensitive fork, ownership and patch-response expectations enter the
release support policy rather than hiding in `third_party/`.

## 10. Release license gate

A release candidate passes only when all of the following are true:

- root and subpackage licenses match the boundary in section 3;
- BSL version and Change Date parameters are correct and archived;
- SDK package metadata and generated headers say Apache-2.0;
- every final-image file is first-party, package-owned, or explicitly inventoried;
- license scan has no unresolved findings;
- notices render successfully and contain required full license texts;
- corresponding-source bundle passes offline resolution and rebuild spot checks;
- SBOM/package/source/notices inventories agree with the signed image digest;
- x11vnc, websockify, noVNC, xdotool, and all modified copyleft components have
  exact source and patch coverage;
- published artifacts remain accessible from a clean, unauthenticated environment;
- an engineer other than the artifact author signs the compliance review record.

## 11. Implementation checklist

- [ ] Add directory-specific Apache-2.0 licenses before the protocol/SDK crates land.
- [ ] Add SPDX/copyright policy and a formatter/checker for source headers.
- [ ] Commit dependency/license allowlists and exception schema.
- [ ] Build final-image `dpkg` and unowned-file inventory tooling.
- [ ] Build binary-to-source resolver against the pinned Debian snapshot.
- [ ] Generate deterministic notice, package, and source manifests.
- [ ] Assemble and offline-verify corresponding-source OCI/release artifact.
- [ ] Add Cargo, Python, JavaScript/static-asset, font/theme, and container scans.
- [ ] Add boundary test that rejects BSL implementation files in SDK packages.
- [ ] Add release test for BSL parameters and four-year maximum.
- [ ] Publish SBOM/provenance/source/notices together and bind each to the digest.
- [ ] Obtain counsel review before the first public production release.

## 12. Primary references

- [Xenoteer root license](../LICENSE)
- [Business Source License 1.1 text and usage guidance](https://mariadb.com/bsl11/)
- [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)
- [Debian redistribution guidance](https://www.debian.org/doc/manuals/debian-faq/redistributing.en.html)
- [Debian license information](https://www.debian.org/legal/licenses/)
- [Debian package search, including source packages](https://www.debian.org/distrib/packages)
- [Debian snapshot service](https://snapshot.debian.org/)
- [x11vnc upstream repository and license](https://github.com/LibVNC/x11vnc)
- [noVNC upstream repository and license](https://github.com/novnc/noVNC)
- [websockify upstream repository and license](https://github.com/novnc/websockify)
- [x11rb crate documentation](https://docs.rs/x11rb/latest/x11rb/)
- [Cargo package license manifest field](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields)
- [cargo-deny license checking](https://embarkstudios.github.io/cargo-deny/checks/licenses/cfg.html)
- [SPDX specification](https://spdx.github.io/spdx-spec/v3.0/)
- [CycloneDX specification](https://cyclonedx.org/specification/overview/)
- [BuildKit SBOM/provenance attestations](https://docs.docker.com/build/metadata/attestations/)
