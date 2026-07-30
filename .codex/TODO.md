# Xenoteer implementation progress

Plan: `plans/15-phased-implementation.md`

## Phase 0 — foundations and risk spikes

- [x] Rust workspace, policies, protocol/core/server/CLI skeletons
- [x] Schema generation and golden validation
- [x] Typed configuration merge/validation/redaction
- [x] CI, formatting, lint, tests, dependency/license policy
- [x] Multi-stage image, s6 skeleton, user/runtime paths, readiness
- [x] X11/XTEST/event-loop/XKB/AT-SPI/capture/viewer/browser spikes
- [x] Phase 0 full verification and handoff

## Phase 1 — native physical input kernel

- [x] Core geometry/interpolation/state/effect algorithms and property tests
- [x] Dedicated XTEST input actor and cleanup/poison behavior
- [x] Keyboard mapping, physical text, chord/sequence behavior
- [x] Fixture/diagnostic conformance and phase verification

## Phase 2 — deterministic desktop container

- [x] Complete s6/Xvfb/D-Bus/AT-SPI/XFCE/viewer service graph
- [x] Deterministic standard/bare desktop profiles
- [x] GTK/Qt/browser application profiles and fixtures
- [x] Development/hardened runtime profiles and phase verification

## Phase 3 — raw control plane and process lifecycle

- [x] Coordinator, lease, ledger, deadlines, generation, event hub
- [x] Authenticated HTTP/WebSocket protocol and limits
- [x] Daemon-only bearer credential ingestion before exposing `/v1`: GUI
      applications share UID 1000 for X11/D-Bus and must never receive or be
      able to reopen the API token file; use a root/external gateway handoff,
      close the plaintext source before app launch, and retain only a keyed hash
- [x] Managed application lifecycle
- [x] Raw input integration, black-box verification, and enforcement that
      temporary keyboard mappings require an exclusive controller lease plus
      server-side view-only viewer authority

## Phase 4 — observation, windows, clipboard, capture, viewer

- [x] X11 observation/window identity, query, control, waits
- [x] Clipboard and text strategy engine
- [x] Capture, DAMAGE, artifacts, and viewer gateway
- [x] Public APIs and phase verification

## Phase 5 — AT-SPI semantic automation

- [x] AT-SPI actor/cache/reconciliation
- [x] Elements/selectors/waits/correlation
- [x] Semantic and physical element actions
- [x] Bounded custom/deep/oversized-name/reconnect/event-flood coverage, with
      malformed parent/cycle behavior covered at the pure model/unit boundary
- [x] Coherent exact-image GTK/Qt/Chromium/Firefox matrix, immutable image-ID
      record, no-override 25-minute live gate, closure review, and local commit

## Phase 6 — SDKs and CLI

- [x] Freeze the v1.0 protocol range, status contract, canonical uint64 wire
      encoding, additive-response/closed-request rules, and compatibility
      fixtures
- [x] Finish the Rust SDK transport/TLS/event/artifact surface, error and
      generation fencing, cancel-safe command handles, explicit leases, domain
      objects, documentation, and adversarial tests
  - [x] Preserve the exact redacted command submission across ambiguous send or
        local cancellation; expose its ID before I/O and never replay implicitly
  - [x] Prove filtered event subscription acknowledgement, bounded frames,
        heartbeat/reconnect, resync continuation, slow-consumer signaling, and
        permanent-error termination
  - [x] Add endpoint-specific JSON limits plus bounded artifact
        upload/download/delete and exact scope/digest validation
  - [x] Close lease/client lifecycle, response-fencing, viewer-origin, and
        redacted-Debug gaps found by the independent review
- [x] Finish the v1-supported `xenoteerctl` command tree, JSON/JSONL/binary output
      discipline, stable exit codes, safe token sources, and comprehensive
      `doctor`
  - [x] Eliminate token-file path-swap races with descriptor-based checks and
        prove command-ID-before-I/O, diagnostic failure exits, and serialization
        failure behavior
- [x] Implement and independently compile/test/package-inspect the TypeScript
      SDK
- [x] Finish and independently test/package-inspect the Python SDK
- [x] Independently harden the TypeScript and Python SDKs against the same
      ambiguity, lifecycle, reconnect, framing, scope, and redaction invariants
  - [x] Repair closure-audit event defects across SDKs: admitted-only resume
        cursors, active-subscription correlation, true sequence-regression
        failure, replay-boundary advancement, and queue-independent terminal
        resync delivery
  - [x] Reject malformed known status capabilities/reasons and pin every direct
        adapter boundary to the exact frozen protocol/corpus identity
- [x] Add the language-neutral v1 conformance corpus and deterministic validator
- [x] Replace narrative scenario/redaction cases with concrete machine fixtures
      and prove every adapter fails on mutated fixtures or incorrect runtime
      behavior instead of deriving assertions from labels
  - [x] Replace remaining manufactured negotiation, generation-fence,
        reference-lifecycle, and redaction outcomes identified by the closure
        audit with observed public-SDK behavior
- [x] Close independently reproduced SDK gaps: real CLI doctor viewer/browser
      probes, TypeScript inbound-event/permanent-error/stream-redaction handling,
      and Python streaming-upload/stale-handle/client-close lifecycle behavior
- [x] Wire Rust/CLI/TypeScript/Python conformance and package-content gates into
      CI, including proof that no BSL server implementation enters an Apache
      package
  - [x] Add content-level copied-BSL rejection to the Rust archive verifier
- [ ] Run every public quick-start against one immutable release-candidate image
      and record the exact image IDs
  - [x] Reproduce and fix the cross-language Phase-6 example deadline
        contradiction: the examples configured a 5-second transport deadline
        around 10-30-second server long-poll waits, and the exact Rust crate
        timed out after launch before window resolution
    - [x] Make the regression fail closed for every transport constructor,
          every server wait, and unsafe overall-deadline changes rather than
          counting only today's configured timeout expressions
  - [x] Fix the recurring viewer monitor's incomplete RFB negotiation, which
        makes TigerVNC blacklist loopback after five probes and intermittently
        degrades readiness/fails the immutable-image viewer gate
  - [x] Add the staged crate/npm/wheel/sdist installation gate, exact
      source-tree/image identity fence, no-override admission, per-variant
      typed-auth failure proof, and bounded cleanup contracts
    - [x] Prove the derived fixture did not shadow the production daemon or
        first-party runtime/config by copying and hashing inherited files from
        stopped base/fixture containers; labels and a layer prefix alone are
        insufficient
  - [x] Ship one canonical ten-behavior executable in each public package and
        require artifact-only execution, fresh per-variant fixture state,
        derived-to-production image ancestry, explicit view-only origin policy,
        real failed-postcondition screenshot evidence, and cleanup
  - [x] Add content-private exact semantic Unicode verification for unprotected
      text targets, retain length-only protected-field behavior, and require
      all three package examples to prove the exact-verification evidence
    - [x] Bound hostile AT-SPI `Text.GetText` replies at the earliest zbus
        boundary before typed decoding or a second content allocation; retain
        and document zbus's unavoidable upstream 128 MiB raw-message cap
  - [x] Correct Python application-argument and nested text-target SDK wire
      serialization with focused regressions
  - [x] Close adversarial package-example findings: bounded Rust scoped-control
      cleanup after renewal failure, honest cancellation semantics, complete
      viewer-ticket metadata proof, and cleanup that preserves independent
      artifact/process failures
  - [ ] Build the final coherent Phase 6 image, run the gate against its exact
        immutable ID, and record only that successful image/package identity
- [ ] Complete Phase 6 closure review, update implementation details, and commit
      the verified phase boundary locally without pushing

## Phase 7 — hardening and first release

- [ ] Add pre-header connection admission, header/read/idle limits, reserved
      health/shutdown capacity, and raw-socket slowloris/flood/recovery tests
- [ ] Add atomic runtime token-set reload, metadata/expiry/revocation, scoped
      principals, and deterministic active-WebSocket revocation behavior
- [ ] Add structured security audit/metrics surfaces and one comprehensive
      canary scan across logs, status, problems, metadata, and metrics
- [ ] Complete the cross-subsystem fault-injection matrix and parser/state-machine
      fuzz targets with checked-in seeds and short blocking replay gates
- [ ] Add reliability/performance/leak harnesses for 10,000-node cold snapshots,
      selector p95, event lag, cache RSS, 100 app cycles, and active browser/
      viewer/action soaks, including a deliberate leak mutation proof
- [ ] Produce deterministic image/language SBOM, source, notice, checksum, and
      offline-verification bundles, plus provenance/signing/release workflows
- [ ] Close container release metadata/profile/binary gaps, including the stale
      phase-2 label, OCI license declaration, and planned `xenoteerctl` artifact
- [ ] Write and execute-check TLS/proxy, token rotation, resource, persistence,
      upgrade/rollback, monitoring, incident, viewer kill-switch, and
      digest-pinned operations documentation
- [ ] Run every locally achievable Phase-7 release gate; record genuine external
      environment gates (OIDC/published registry/protected tags/24-hour soak/
      supported-host LSM) as unverified rather than simulating success

## Current constraints

- Docker Engine 29.1.3 and Docker Compose 5.3.1 are installed and verified. The
  current shell requires `sudo docker` until group membership is refreshed.
- Rust/Cargo 1.97.1, Xvfb, D-Bus, XKB, AT-SPI, GTK, cargo-deny, cargo-audit,
  shellcheck, noVNC, websockify, and the viewer/browser test dependencies are
  installed. cargo-nextest is not installed and is not a Phase 0 requirement.
- Repository Cargo configuration caps builds at two jobs. Heavy build and test
  gates run sequentially at reduced CPU/I/O priority; desktop matrix and idle
  soak containers are capped at two CPUs.
- Chromium's supported sandbox path requires the pinned, narrow seccomp profile,
  a private `/dev/shm` of at least 4 GiB, and the documented seven-capability
  allowlist in the hardened s6-overlay runtime. `KILL` is retained solely so
  root PID 1 can stop supervised UID 1000 payloads cleanly.
