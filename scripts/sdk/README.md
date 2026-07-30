<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Public SDK release qualification

`scripts/container/qualify-phase6.py` is the sole canonical Phase 6
release-candidate gate. It admits one exact production image and one exact
desktop-app fixture image, then runs the frozen seven-lane qualification
serially. Both arguments must be distinct lowercase immutable
`sha256:<64 hex>` image IDs; tags and abbreviated digests are rejected.

Run the Phase 6 package/unit gates first so Cargo's offline cache, the exact npm
development tree, and the locked Python build/runtime dependencies are already
available. Build the release-candidate image from the final unchanged tree,
build its fixture from that exact production image, resolve both images to
immutable IDs, then run:

```sh
sudo /usr/bin/python3 scripts/container/qualify-phase6.py \
  sha256:<production-image-64-hex-digest> \
  sha256:<fixture-image-64-hex-digest>
```

If the Phase 6 Python tools live in a virtual environment, invoke this script
through `sudo` with that environment's absolute Python executable instead of
`/usr/bin/python3`. Do not wrap the complete command in the heavy-build lock:
the orchestrator proves that lock is initially free, owns it only around the
lanes that require outer serialization, and leaves the three self-locking lanes
free to acquire it themselves. The canonical orchestrator is deliberately
root-only; rootless support for individual development gates does not make a
rootless seven-lane run release qualification.

Lane 7 discovers its host Node/npm pair from the invoking account recorded in
the local password database, never from caller-supplied `HOME`, `PATH`,
`NVM_DIR`, `NVM_BIN`, shell initialization, or NVM aliases. If
`~/.nvm/versions/node` exists, every entry claiming supported Node major 22 or
24 must be a canonical, non-symlinked `vMAJOR.MINOR.PATCH` installation with a
trusted `bin/node` and `bin/npm`; npm's reviewed symlink target must remain
inside that same version root. The orchestrator bounds the NVM inventory to 64
entries, performs only filesystem shape/trust checks, then selects the highest
semantic version; it never executes invoking-user Node code at the root
orchestration boundary. When no supported NVM entry exists, fallback is allowed
only when one fixed trusted system directory contains both Node and npm.

The selected directory is carried into lane 7 through the dedicated
`XENOTEER_PACKAGE_BUILD_PATH` channel while every root Docker/Git subprocess
retains the fixed `/usr/sbin:/usr/bin:/sbin:/bin` PATH and a minimal
password-database-derived environment. At the start of package use, the public
runner binds only the first selected directory—deletion, replacement, or a raw
PATH alias cannot fall through to Cargo or a later system directory. For NVM,
it additionally requires a regular non-symlink `node`, one reviewed `npm`
symlink, non-symlinked trusted path components to its in-root regular target,
and an exact bounded `#!/usr/bin/env node` wrapper. It then drops to the
invoking build identity for one output-bounded, process-group-bounded
`node --version` probe and requires the runtime to equal the selected NVM
directory. That same immutable node/npm pair and PATH snapshot reaches package
assembly, archive installation, and installed quick-start verification.

The shared heavy-build lock is the one intentional ownership exception:
after validating `SUDO_UID`/`SUDO_GID` against the checkout and local account,
the root orchestrator opens the sticky parent through an anchored directory
descriptor and normalizes the lock inode to that verified parent's UID, the
invoking user's primary GID, and mode `0660`. The parent itself is never
reowned, and an existing lock must be a single-link regular inode before any
ownership or mode change. Thus a pre-existing invoking-user-owned parent
produces a user:user lock, while a freshly root-created parent produces a
root:invoking-group lock. In either case the file owner matches the directory
owner, which satisfies Linux `fs.protected_regular` when util-linux
`flock PATH COMMAND` adds `O_CREAT`; group DAC also lets the invoking user open
the lock. The lock has no secret content or world access; the session lock and
all evidence remain root-private.

The orchestrator rejects source drift, dirty or mismatched image provenance,
daemon binary overrides, concurrent qualification, and the first failing or
timed-out lane. It runs every child at low CPU/I/O priority in its own bounded
process group and records combined output in a private, exclusive evidence
directory below `/tmp/xenoteer-phase6-qualification-evidence`. During a
run, `attempt.json` is atomically updated with each lane's status, duration,
exit status, and log digest. Only all seven admitted lanes can create
`qualification.json`; that final manifest is the sole success authority, while
the hash-bound `attempt.json` remains non-authoritative in `lanes-passed`
state. Preflight failures before lane 1 create no attempt and may be retried
with the same exact pair. Once any lane starts, a rejection or interruption
invalidates that image pair for release: there is no resume, retry, or skip
mode. Correct the cause, rebuild both images, and begin a new complete attempt
with new exact IDs.

`scripts/sdk/test-public-quickstarts.py` is lane 7 and remains useful for
focused package-path debugging. It is not, by itself, release qualification.
That lane assembles and installs the actual `.crate`, npm `.tgz`, Python wheel,
and Python source distribution, then executes the package-native behavior
program from each installation. Each artifact-contained example must complete
the same ten behavior proofs; a status-only smoke test is not accepted.

Before packaging, lane 7 verifies the fixture label, exact base-image label,
complete production-layer prefix, and the production image's
`com.aeor.xenoteer.source-tree.sha256` against the current tree. Every
container is started by the exact fixture ID and checked again. Any identity,
ancestry, or source-tree change fails the run.

The gate also:

- rejects `XENOTEERD_BINARY_OVERRIDE` and
  `XENOTEER_TEST_DAEMON_BINARY` even when present with an empty value;
- resolves the Rust SDK and protocol only from safely extracted staged crate
  archives, never from `crates/`;
- resolves Node and Python imports only from the isolated npm/wheel/sdist
  installations, never from `packages/`;
- loads the Rust, Node, and Python executable examples only from those
  extracted or installed artifacts, never from `scripts/sdk/quickstarts`;
- proves each installed variant returns the typed HTTP 401 authentication
  failure for a bounded invalid-token attempt before its successful behavior
  run;
- creates a fresh fixture container for each of the crate, npm tarball, wheel,
  and sdist variants;
- explicitly enables only the exact `https://viewer.example` viewer origin,
  launches the image-resident GTK fixture as desktop UID 1000, and leaves
  broker-managed application launch to the public SDK's registered `xmessage`
  profile;
- requires canonical ordered evidence for status/capabilities, scoped
  lease/launch, exact window/element resolution, semantic invoke, smooth
  physical click with postcondition, exact Unicode strategy evidence,
  screenshot after an actual bounded failed postcondition, known-command
  reconnect, stale reference after restart, and view-only browser ticket;
- caps host work at low CPU/I/O priority with Cargo limited to two jobs;
- caps the container at two CPUs, 6 GiB memory, 512 PIDs, and 4 GiB shared
  memory;
- bounds every external operation and removes the container and temporary
  installation tree on both success and failure;
- checks both bearer canaries are absent from child output and container logs.

Only a completely successful canonical seven-lane run prints the exact fixture
image, production image, source-tree, and package SHA-256 identities. Copy
those values into the Phase 6 implementation record only from
`qualification.json`; direct lane output, unit tests, and an older image are
not substitutes.

The Apache-2.0 executable sources live inside their public packages:
`crates/xenoteer-sdk/examples/phase6_behaviors.rs`,
`packages/typescript/examples/phase6-behaviors.mjs`, and
`packages/python/src/xenoteer/examples/phase6_behaviors.py`. The release
orchestration and its tests remain under the repository's server license.
