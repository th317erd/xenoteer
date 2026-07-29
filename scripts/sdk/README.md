<!-- SPDX-License-Identifier: BUSL-1.1 -->

# Public SDK release qualification

`test-public-quickstarts.py` is the Phase 6 release-candidate gate for the
public Rust, TypeScript, and Python installation path. It is deliberately
separate from source-tree unit and conformance tests: the gate assembles and
installs the actual `.crate`, npm `.tgz`, Python wheel, and Python source
distribution, then executes the minimal public status quick-start from each
installation.

Run the Phase 6 package/unit gates first so Cargo's offline cache, the exact npm
development tree, and the locked Python build/runtime dependencies are already
available. Build the release-candidate image from the final unchanged tree,
then run:

```sh
sudo nice -n 15 ionice -c 3 \
  timeout 15m python3 scripts/sdk/test-public-quickstarts.py xenoteer:phase6-rc
```

If the Phase 6 Python tools live in a virtual environment, invoke this script
with that environment's Python executable instead of `python3`.

The supplied tag is navigation only. Before packaging, the gate resolves it
once to a lowercase `sha256:` image ID, reads the image's
`com.aeor.xenoteer.source-tree.sha256` label, and requires that exact identity
to match the current tree. The container is then started by ID, and its
recorded image ID is checked again. Any source-tree change during package
assembly or execution fails the run.

The gate also:

- rejects `XENOTEERD_BINARY_OVERRIDE` and
  `XENOTEER_TEST_DAEMON_BINARY` even when present with an empty value;
- resolves the Rust SDK and protocol only from safely extracted staged crate
  archives, never from `crates/`;
- resolves Node and Python imports only from the isolated npm/wheel/sdist
  installations, never from `packages/`;
- proves each installed variant returns the typed HTTP 401 authentication
  failure for a bounded invalid-token attempt before its successful status
  request;
- caps host work at low CPU/I/O priority with Cargo limited to two jobs;
- caps the container at two CPUs, 6 GiB memory, 512 PIDs, and 4 GiB shared
  memory;
- bounds every external operation and removes the container and temporary
  installation tree on both success and failure;
- checks both bearer canaries are absent from child output and container logs.

Only a completely successful run prints the exact image, source-tree, and
package SHA-256 identities. Copy those values into the Phase 6 implementation
record only after this exact-image run; unit tests and an older image are not
substitutes.

The language quick-start sources live under `quickstarts/`. They are
Apache-2.0 examples; the release orchestration and its tests remain under the
repository's server license.
