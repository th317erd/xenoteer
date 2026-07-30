<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Public SDK release qualification

`test-public-quickstarts.py` is the Phase 6 release-candidate gate for the
public Rust, TypeScript, and Python installation path. It is deliberately
separate from source-tree unit and conformance tests: the gate assembles and
installs the actual `.crate`, npm `.tgz`, Python wheel, and Python source
distribution, then executes the package-native behavior program from each
installation. Each artifact-contained example must complete the same ten
behavior proofs; a status-only smoke test is not accepted.

Run the Phase 6 package/unit gates first so Cargo's offline cache, the exact npm
development tree, and the locked Python build/runtime dependencies are already
available. Build the release-candidate image from the final unchanged tree,
then run:

```sh
sudo nice -n 15 ionice -c 3 \
  timeout 15m python3 scripts/sdk/test-public-quickstarts.py xenoteer:desktop-apps-test
```

If the Phase 6 Python tools live in a virtual environment, invoke this script
with that environment's Python executable instead of `python3`.

The supplied tag is navigation only and must identify the desktop-app fixture
image. Before packaging, the gate resolves the fixture and its recorded
production base to lowercase immutable `sha256:` IDs. It verifies the fixture
label, exact base-image label, complete production-layer prefix, and the
production image's `com.aeor.xenoteer.source-tree.sha256` against the current
tree. Every container is then started by the exact fixture ID and checked
again. Any identity, ancestry, or source-tree change fails the run.

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

Only a completely successful run prints the exact fixture image, production
image, source-tree, and package SHA-256 identities. Copy those values into the
Phase 6 implementation record only after this exact-image run; unit tests and
an older image are not substitutes.

The Apache-2.0 executable sources live inside their public packages:
`crates/xenoteer-sdk/examples/phase6_behaviors.rs`,
`packages/typescript/examples/phase6-behaviors.mjs`, and
`packages/python/src/xenoteer/examples/phase6_behaviors.py`. The release
orchestration and its tests remain under the repository's server license.
