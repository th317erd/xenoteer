# Build, release, and operations

## 1. Artifact set

One release produces by immutable revision/digest:

- OCI image `ghcr.io/th317erd/xenoteer` for supported platforms;
- `xenoteerd`, `xenoteerctl`, and optional SDK Rust artifacts as image contents;
- Rust SDK crate, npm package, and Python package when their phases are complete;
- protocol JSON schemas/OpenAPI/examples;
- SPDX or CycloneDX SBOM for image and language packages;
- SLSA-compatible provenance/build attestation;
- image signature/identity attestation;
- third-party notices, license manifest, and corresponding-source instructions/
  bundle for redistributed copyleft binaries;
- checksums and release notes/migration notes.

Artifacts are linked by source commit, semantic version, protocol range, image
digest, and SBOM hash. Tags are navigation; digest is identity.

## 2. Repository/build layout

```text
Dockerfile
docker/
  rootfs/etc/s6-overlay/s6-rc.d/...
  profiles/standard/...
  profiles/bare/...
  entrypoint-assets/...
  compose.dev.yml
  compose.hardened.yml
build/
  base.lock
  downloads.lock
  debian-packages.lock
  rust-toolchain.lock (or rust-toolchain.toml)
  licenses/...
.github/workflows/
  ci.yml
  nightly.yml
  release.yml
```

Locks are machine-readable and reviewed. A helper may update them, but release
workflow never resolves "latest" silently.

## 3. Multi-stage Docker build

Conceptual stages:

1. `rust-builder`: pinned Rust image/toolchain, dependency cache mounts, builds
   locked release binaries and tests build metadata.
2. `viewer-assets`: verifies/extracts pinned noVNC/websockify artifacts or Debian
   source/package choice.
3. `source-licenses`: gathers Cargo/Debian license/source metadata and notices.
4. `runtime`: pinned Debian stable slim, installs exact declared package set,
   s6-overlay, desktop profiles, binaries, viewer assets, licenses.
5. optional `test`: release runtime plus fixture applications/debug tools; e2e
   verifies release base before test additions.

Build principles:

- `# syntax=docker/dockerfile:<pinned-compatible>` and BuildKit required;
- base `FROM name@sha256:digest`;
- `ARG TARGETARCH/TARGETPLATFORM` used only through validated mapping for s6
  artifact names;
- downloaded archives verified before extraction and removed same layer;
- apt lists removed; no compiler/header/cache in final unless runtime-required;
- no curl-to-shell;
- deterministic ownership/modes in COPY;
- runtime image has no Cargo registry/source tree except license/source offer
  artifacts deliberately included;
- `USER` remains compatible with s6 root preinit, while services drop privileges.

## 4. Dependency pin/update process

### Debian

- Record base digest, snapshot timestamp, package names/versions/architectures,
  source package versions, and repository suites/components.
- Rebuild candidate weekly and immediately for relevant critical advisories.
- Test current candidate before advancing locks.
- Do not `apt upgrade` an old snapshot ad hoc; advance snapshot/base coherently.
- Keep enough metadata/source retrieval references to meet redistribution terms
  even if upstream snapshot/index changes.

### Rust

- Cargo.lock committed for all binaries.
- Depend on registry releases; git dependencies require immutable commit,
  documented necessity, license/source inclusion, and temporary review date.
- Dependabot/Renovate-style PRs run full affected matrix, never auto-merge major
  backend/protocol/security changes.
- Track MSRV after release and test pinned MSRV plus current stable if they differ.
- Review duplicated security/native crates with `cargo tree`/cargo-deny.

### Downloaded tools/assets

- s6-overlay architecture tarballs: version + SHA-256 per file.
- noVNC/websockify: prefer tagged release archives or Debian packages; pin and
  verify, record license/content.
- Never pull GitHub `main`/`master` in release build.
- Vendor only when availability/reproducibility needs justify the maintenance
  burden; preserve upstream license/history notices.

## 5. Versioning and compatibility

- Product/image/server use SemVer after `1.0`; pre-1.0 may use `0.y.z` but release
  notes still classify breaking changes.
- Protocol has independent major/minor range.
- SDK versions declare supported protocol/server range; they need not equal
  server version, though coordinated releases may share a train.
- Image labels/status expose server version, git revision, protocol range, build
  timestamp, base digest, profile revision, dependency lock hash.
- Desktop profile/config schema has a version. Persistent home/browser profiles
  record creating image/profile version and migrations are explicit.

Tag policy:

```text
v1.2.3       immutable release tag policy (registry protection; digest recorded)
1.2          moving compatible patch pointer
1            moving major pointer
edge         main builds, never production promise
sha-<commit> immutable CI artifact
```

Avoid `latest` in documentation. If published, define it as current stable and
still show digest pinning for production.

## 6. CI build trust

GitHub Actions workflow:

- minimal `contents: read` by default;
- release job separately grants `packages: write`, `id-token: write`, and
  `attestations: write`;
- third-party actions pinned to commit SHA and periodically reviewed;
- PRs from forks cannot access release secrets or publish artifacts;
- BuildKit secret mounts and OIDC short-lived credentials;
- protected tags/environments require review as appropriate;
- build/push by digest, then sign/attest that digest;
- release does not reuse untrusted PR cache for executable build steps without
  cache isolation/verification.

Generate GitHub/Docker provenance and SBOM attestations. Use cosign keyless OIDC
signature (or organization KMS) and document identity/issuer verification. Never
verify with `--check-claims=false` in recommended instructions.

## 7. Multi-architecture plan

Initial supported platform is `linux/amd64` until full desktop/browser/test matrix
passes. `linux/arm64` becomes supported only after:

- all Debian/runtime packages and s6 artifacts exist;
- Rust cross/native binary works;
- Xvfb/XFCE/input/capture/AT-SPI/clipboard fixtures pass natively;
- packaged Chromium/Firefox/QtWebEngine availability and sandbox proven;
- visual baselines separated by architecture;
- viewer soak passes.

Docker documents QEMU emulation as slower, especially for compilation. Use Rust
cross-compilation/native multi-node builders for release where practical; QEMU
may build/test smoke but does not replace native runtime compatibility. Manifest
list is published only for platforms that passed.

## 8. Release pipeline

### Candidate

1. clean protected source tag/revision;
2. verify locks/licenses/config/schema clean;
3. full CI/security/performance/soak gates current;
4. build image independently for each platform;
5. run black-box tests against exact local/registry digest;
6. scan final digest and create SBOM/provenance;
7. compare package/content manifest to expected allowlist;
8. stage SDK packages and run cross-version conformance;
9. generate release notes including defaults, migration, known limitations,
   security fixes, dependency/base changes, and license source procedure.

### Publish

1. push immutable platform manifests/digests;
2. attach SBOM/provenance/license/source artifacts;
3. sign digest with release identity;
4. verify signature/attestations from clean job;
5. publish SDK packages with provenance where ecosystems support;
6. update moving tags only after immutable artifacts verify;
7. publish GitHub release containing digests/checksums/links;
8. run clean-host install/quick-start smoke from public registry.

### Rollback/revocation

Moving tag can return to a prior secure digest, but immutable release tags are not
silently rewritten. Publish an advisory/revocation note for bad digest and mark
release. Operators pinning digest need explicit notification; status/feeds
provide affected range. Never delete evidence needed for incident analysis or
license compliance unless legally required.

## 9. Reproducibility

Goal: explain and largely reproduce inputs, not promise bit-for-bit output before
measuring all timestamps/toolchains/packages.

- Pin base/download/toolchain/Cargo/Debian snapshot.
- Set deterministic source date/version metadata where tools support.
- Avoid embedding volatile build path/user/time except declared OCI creation
  metadata/attestation.
- Run periodic two-builder comparison of binaries/image file manifests and
  report differences.
- Prefer reproducible Debian binaries from snapshot over rebuilding the distro.
- Provenance is still required even if bit-identical.

Document the actual level: `input-reproducible`, `file-manifest-reproducible`, or
`bit-reproducible` only after evidence.

## 10. Operations configuration

### Required operator inputs

- API auth token file/provider;
- listener bind and trusted proxy/TLS arrangement;
- home persistence policy;
- workspace mount/ownership;
- `/dev/shm` size (>=1 GiB; 2 GiB recommended browser-heavy);
- CPU/memory/PID/fd/log limits;
- egress/network policy external to container;
- artifact directory/retention if enabled;
- desktop profile/locale/timezone immutable at boot.

No hidden dependency on systemd, host X11, `/dev/input`, GPU, or privileged mode.

### Recommended Compose profile

- restart policy on nonzero critical exit;
- API published to host loopback or protected proxy network;
- `shm_size: 2gb`;
- healthcheck uses `/readyz` with start period covering desktop boot;
- stop grace exceeds s6/daemon cleanup;
- read-only root plus tmpfs in hardened example;
- capabilities dropped and default seccomp/LSM;
- named ephemeral/persistent home and workspace mounts;
- logging rotation at runtime.

Kubernetes manifests/operator are deferred. Generic container requirements and
health semantics are documented so an operator can integrate without privileged
assumptions; arbitrary random UID is explicitly unsupported initially.

## 11. Health, metrics, and alerts

Health meanings are in [01](01-container-runtime.md). Metrics endpoint is
authenticated or bound to a private operations listener. Suggested alerts:

- desktop not ready or generation churn;
- input actor poisoned/reset failure;
- command queue saturation/latency/error spike;
- lease stuck/conflict spike;
- AT-SPI reconnect/cache budget failures;
- viewer crash loop;
- process count/PID/fd/memory near limit;
- artifact disk near quota;
- auth failures/rate limit spike;
- image/dependency advisory affecting deployed digest.

Avoid alerts on a single expected application refusal. Dashboards distinguish
infrastructure/backend health from target application behavior.

## 12. Persistence, backup, and reset

Default home is ephemeral. Persistent home is useful for browser/app profiles but
introduces secrets, version drift, saved state, and test nondeterminism.

- Persistent volume has profile schema/version marker.
- Startup refuses incompatible newer profile; supported migration is backed up
  transactionally before mutation.
- `reset-profile` is an explicit offline/operator operation with exact target and
  recovery warning; never automatic recursive deletion.
- XFCE saved sessions remain disabled even on persistent home.
- Backup/restore is external volume responsibility; Xenoteer provides quiesce/
  shutdown guidance, not live consistent backup in release one.
- API token/signing secrets do not live in the desktop home backup.

## 13. Upgrade procedure

1. Read release notes and compare config/profile/protocol compatibility.
2. Pull by digest and verify cosign identity/issuer, provenance, and SBOM policy.
3. Run `xenoteerctl doctor` against candidate in a new container with copy of
   persistent profile where used.
4. Drain current API: stop new commands, revoke lease/reset input, terminate or
   preserve external workflow state.
5. Stop old container cleanly; never attempt live X desktop handoff.
6. Start new container; generation changes by design.
7. Clients reconnect/resync/reacquire refs/lease.
8. Roll back container+profile backup if migration/readiness fails.

In-flight commands are not migrated. Orchestrators must treat outcome as known
from terminal ledger before drain or unknown after forced restart.

## 14. Maintenance cadence

- Weekly dependency/base candidate and vulnerability triage.
- Monthly routine image rebuild even without Xenoteer code change.
- Immediate critical reachable security rebuild.
- Quarterly restore/rollback/source-offer/signature verification drill.
- Each release rechecks x11vnc maintenance/security status and replacement
  candidates.
- Deprecation announced at least one minor release before removal within major,
  unless security requires immediate disable.

## 15. Release/operations acceptance

- Clean public digest boots under documented one-command dev example.
- Hardened example passes read-only/capability/sandbox/listener tests.
- Image manifest contains no compiler/cache/secrets/unexpected daemon.
- Each supported architecture passed native behavioral suite.
- SBOM/provenance/signature verify from clean environment.
- Corresponding source/license notices available and tied to digest.
- SDK packages/examples match protocol and contain only intended licensed files.
- Upgrade/rollback/profile migration tested.
- Alert/health behavior exercised by killing required/optional services.

## 16. Primary references

- [Docker multi-platform builds](https://docs.docker.com/build/building/multi-platform/)
- [Docker build attestations](https://docs.docker.com/build/metadata/attestations/)
- [Docker provenance attestations](https://docs.docker.com/build/metadata/attestations/slsa-provenance/)
- [Docker GitHub Actions SBOM/provenance](https://docs.docker.com/build/ci/github-actions/attestations/)
- [GitHub artifact attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)
- [Cosign signing containers](https://docs.sigstore.dev/cosign/signing/signing_with_containers/)
- [Cosign verification](https://docs.sigstore.dev/cosign/verifying/verify/)
- [Debian snapshot service](https://snapshot.debian.org/)
- [Docker Compose service options, including `shm_size`](https://docs.docker.com/reference/compose-file/services/)
