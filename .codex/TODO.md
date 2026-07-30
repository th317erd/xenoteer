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
  - [x] Commit broker-authenticated managed-process correlation into the
        revision-fenced window model so public query, resolve, and wait
        selectors can match it atomically
    - [x] Prove late correlation wakes an already-registered wait without a
          lost event, while stale revisions, XID/PID reuse, malformed broker
          replies, no-match, and unavailable correlation remain fail-closed
    - [x] Invalidate committed high-confidence correlation before process-exit,
          broker replay-gap/resync, disconnect, and reconnect effects can be
          observed; keep shutdown/cancellation bounded
    - [x] Gate correlation commits on a shared lifecycle-subscription authority
          epoch that starts unavailable, enables only after replay/live
          handoff, and disables before any stream ambiguity or reconnect
    - [x] Fence singleflight completion with the actor-published post-commit
          change sequence so a raw/accessibility mutation between replace and
          leader completion can never cache stale success or strand a waiter
    - [x] Preserve exact-birth/same-PID correlation across individual raw
          metadata updates, clear it on PID/birth change, and publish every
          shared-model mutation before its response becomes observable
    - [x] Permit ordinary fail-open projection only after correlation
          invalidation is confirmed; an unavailable/full actor queue must never
          leak previously committed high evidence
    - [x] Preserve one monotonic semantic wait deadline across correlation
          refresh, actor registration, wake, and any revision recheck rather
          than restarting the caller's timeout
  - [x] Give window/element long polls endpoint-owned handler headroom and
        per-operation Rust/Python SDK deadlines so every legal 300/120-second
        wait can return a typed result instead of racing a generic 30-second
        HTTP 504
    - [x] Preserve the ordinary-handler cutoff, command-wait clamp, exact route
          classification, cancellation, response limits, and TypeScript's
          existing per-operation deadline behavior
  - [x] Close the cross-connection window-state handoff race exposed by the
        coherent-image Phase-4 lane
    - [x] Reproduce fail-first when the raw X11 control connection observes
          converged iconification before the observation/model actor commits
          the matching hidden state
    - [x] Reconcile state/minimize postconditions through the exact
          actor-owned WindowRef without replaying the effect, while preserving
          bounded genuine-nonconvergence and stale-reference behavior
    - [x] Prove the observation actor's bounded snapshot barrier orders both
          successful and nonterminal-failed snapshot round trips behind emitted
          X11 events, including exact event-budget and overflow/resync cases
    - [x] Prevent an event-driven refresh immediately before Destroy/reuse from
          publishing replacement bytes or waking a waiter under the old birth;
          cover unrelated-event instability without dropping the original
          refresh
    - [x] Run focused, workspace, native-X11, and independent review gates
  - [x] Build and exercise diagnostic coherent candidates at source `0c7f9ff`;
        reject them after the first Phase-4 live run exposed the handoff race
        even though the controlled rerun and all other live lanes passed
  - [ ] Build the final coherent Phase 6 image, run the gate against its exact
        immutable ID, and record only that successful image/package identity
    - [x] Reject source `e808258` production `e594ed77` / fixture `529fd865`
          after lane 1 passed first-run but the coordinator's lane-2 outer
          heavy-build lock deadlocked the viewer-denial subgate's nested lock
      - [x] Add a fail-first qualification-runner contract that rejects an
            already-held heavy-build lock before any lane and encodes the exact
            seven-lane order without wrapping the three self-locking lanes
        - [x] Close the real sudo-to-invoking-user util-linux `flock` permission
              boundary; mocked/read-descriptor locking is not sufficient
      - [ ] Commit and push the qualification repair, build a new coherent
            pair, and restart at lane 1
        - [x] Run focused/static source gates, the real shared-lock boundary,
              and independent security/portability review
        - [x] Commit and push the qualification repair
        - [ ] Build a new coherent production/fixture pair and restart the
              canonical seven-lane qualification at lane 1
    - [x] Reject source `c1f5caf` production `424a5e9e` / fixture `7c8ea2bd`
          after lanes 1-4 passed first-run but lane 5 passed the raw immutable
          production ID into a Dockerfile `FROM`, which BuildKit interpreted as
          a registry repository and rejected before noVNC runtime assertions
      - [x] Reproduce the exact immutable-ID build-reference failure before
            editing and map every local-image `FROM` consumer
      - [x] Add a fail-closed ephemeral local-tag handoff that remains bound to
            the exact inspected production ID and cleans up on every exit path
      - [x] Run focused/static gates and independent security/portability
            review, commit/push, build a new pair, and restart at lane 1
        - [x] Remove the deterministic exact-path `/tmp` residue from the
              deliberate reservation-owner-mismatch regression without
              weakening the production helper's fail-closed cleanup
        - [x] Obtain a fresh independent High/Medium-zero review of the
              corrected frozen patch
    - [x] Reject source `439d45e` production `0c50f668` / fixture `0784d251`
          after lanes 1-6 passed first-run but lane 7 rejected before package
          assembly because its sanitized package-tool `PATH` contained Cargo
          and system directories but not the invoking user's only npm/Node
          installation under NVM
      - [x] Reproduce the exact root-runner-to-invoking-user npm discovery
            failure before editing and map every package-tool path producer,
            sanitizer, resolver, caller, test, and documented host prerequisite
      - [x] Admit a deterministic supported Node/npm toolchain without sourcing
            user shell code, trusting arbitrary path input, or weakening the
            existing owner/group/traversal/executable checks
      - [x] Prove Node 22/24 selection, ambiguity, malformed layouts, symlink
            targets, permissions, root/non-root execution, missing tools,
            hostile environment, and exact privilege-drop behavior
      - [x] Run focused/static gates, a real root-to-invoking-user npm build
            probe, and independent security/portability review before committing
            and building one new coherent pair
    - [x] Reject source `b01405a` production `a650e129` / fixture `356225fe`
          after lanes 1-3 passed first-run but the lane-4 host runner exited 77
          before exercising the image because sudo's secure `PATH` hid Cargo
      - [x] Reproduce the event-flood toolchain-ordering failure before editing,
            map sibling Cargo/Rust host runners, and fix the failure class
      - [x] Run focused/static source gates and independent security/portability
            review, including the actual root-to-UID-1000 secure-`PATH` boundary
      - [x] Commit and push the repair, build a new coherent pair, and restart
            qualification from lane 1
    - [ ] Diagnose and close the first-run Phase-5 cursor-bound accessibility
          query failure on source `47b5dbf`; reject production `e8b60118` /
          fixture `a29e8d8b` and do not rerun or accept either image
      - [x] Reproduce the exact failure deterministically before editing
      - [x] Map cursor issuance/validation, snapshot revision and AT-SPI rebuild
            consumers; fix the failure class rather than weakening the live gate
      - [x] Fix and execute-check the packaged Python README's obsolete selector
            examples against the frozen v1 wire contract
      - [x] Fulfill the promised Node and Python support-version CI/package
            matrix, including Node 24 and Python 3.14 if official support
            verification confirms the independent audit
      - [x] Cryptographically hash-lock Python CI dependencies across every
            supported runtime and reject missing/altered/unhashed artifacts
      - [x] Make Python wheel/sdist source verification fail closed on every
            non-Apache or multiple SPDX identifier, not only BUSL
      - [x] Reject duplicate or normalization-alias members in Python wheel/
            sdist archives before allowlist, metadata, or SPDX verification
      - [x] Reject logical package-member collisions after wheel dist-info and
            sdist root/egg-info normalization
      - [x] Make CI contracts inspect executable run blocks, remove duplicate
            Rust SDK/CLI test execution, and correct selector additivity prose
      - [x] Reject CI gate prefix impostors, background execution, and
            shell-masked failures in every required executable run command
      - [x] Require each CI gate as an exact simple run step; reject heredoc,
            uncalled-function, and false-conditional text as evidence
      - [x] Reject YAML plain-scalar continuation lines after every required
            one-line CI `run` command
      - [x] Resolve the Rust/TypeScript/Python client-option drift against the
            normative Phase-6 plan with executable public-API evidence
        - [x] Replace the TypeScript logger's raw identifier-bearing paths with
              closed route templates and cover every HTTP/artifact/WebSocket
              attempt, including streaming completion and failure
        - [x] Give TypeScript and Python one retained, documented HTTP/WebSocket
              adapter policy with bounded reconnect, explicit ownership, failed
              socket cleanup, public exports, and same-policy TLS/proxy guidance
        - [x] Bound failed-connect cleanup of client-owned Python transports
              without replacing the original negotiation/connect failure
        - [x] Add Python's missing safe metadata hook and prove hook/provider/
              transport failures cannot leak secrets or alter SDK outcomes
        - [x] Bound Python HTTP token resolution and the complete initial/
              reconnect WebSocket handshake by one deadline and cancellation;
              reject control-character/oversized hello metadata before I/O
        - [x] Ensure timed-out Python credential providers cannot accumulate
              unkillable worker threads/tasks or stall client/process shutdown
        - [x] Give every Python HTTP/artifact request one client-cancellable
              absolute deadline from token resolution through body/output
        - [x] Reject synchronous Python artifact sinks before I/O so blocking
              callbacks cannot bypass the absolute download deadline
        - [x] Bound Python WebSocket cleanup, established writes, resubscribe,
              heartbeat, and old-socket retirement against blocking adapters
        - [x] Make Python close-once socket ownership identity-safe across ID
              reuse and multiple reconnect generations, with bounded tracking
        - [x] Preserve Python close-once when a factory returns the same live
              physical socket in a later reconnect generation
        - [x] Enforce the async-only provider contract at exported
              `EventSession.connect`, before invocation or socket I/O
        - [x] Use the exact cross-SDK transient WebSocket close allowlist in
              Python and make every other peer disposition terminal
        - [x] Bound TypeScript rotating-token providers by the same absolute
              HTTP/WebSocket operation deadline and cancellation authority
        - [x] Close established TypeScript sockets before reconnect, and make
              reconnect backoff promptly cancellable without double-close
        - [x] Validate TypeScript protocol range and bounded client metadata
              before any HTTP/WebSocket I/O; bound the initial hello frame
        - [x] Make TypeScript WebSocket close classification terminal for
              normal/protocol/data/size codes and transient only for the exact
              reconnectable close set
        - [x] Reject missing/duplicate/malformed/mismatched artifact integrity
              headers before TypeScript yields the first streamed byte
        - [x] Reject case-insensitive caller collisions with SDK-owned
              authorization, framing, acceptance, and digest headers
        - [x] Preserve exact Rust safe-log terminal pairs across timeout,
              client close, and caller cancellation
        - [x] Give Rust HTTP/artifact requests one client-cancellable absolute
              deadline spanning token resolution through response completion
        - [x] Keep Rust HTTP safe-log terminal state pending through bounded
              body collection and decode; fail post-header errors truthfully
        - [x] Bound Rust WebSocket hello/subscription sends by the absolute
              connect deadline and client cancellation
        - [x] Classify Rust pre-welcome 4401/4403/1008 policy closes as exact
              terminal auth/permission errors without reconnect
        - [x] Bound and client-cancel every established-session Rust WebSocket
              heartbeat/pong write
        - [x] Close/drop Rust's failed established WebSocket before reconnect
              backoff or replacement attempts
        - [x] Remove Rust's false callback-panic secrecy promise and prove the
              exact panic-hook versus SDK-error/log isolation boundary
      - [x] Execute, update, or remove the packaged Rust
            `phase3-control-smoke` example so every shipped example is proven
      - [x] Run focused, workspace, static, native, and independent review gates
            before building another coherent image pair
    - [x] Reject source `e63f52d` production `61a92b02` / fixture `79ef2dfe`
          after the first public quick-start lane failed before package
          acceptance because its nested unprivileged build lost the
          user-installed Node toolchain
      - [x] Reproduce the secure-`PATH` failure before changing the runner and
            cover Cargo, npm/Node, and Python package commands under the same
            explicit privilege-drop contract
      - [x] Sanitize executable discovery without rejecting stale host `PATH`
            entries or the target user's primary-group-writable NVM toolchain;
            preserve npm/rustup proxy names and redact constructed HOME/PATH
            diagnostics
      - [x] Stop executing installed Rust/Node/Python quick-starts as root:
            require a non-root UID/GID, clear supplementary groups, use an
            exact environment, keep bearer secrets out of argv, and run from
            the canonical installed artifact root
      - [x] Prove the final boundary with 48 focused/contract tests, a real
            nested-sudo npm/Node probe, a real UID/GID/group/environment probe,
            repository-shadow isolation, and two independent reviews
    - [x] Diagnose and close the first-run Phase-5 live `element_set_text`
          backend failure on fresh source `aff69fa`; do not accept or blindly
          rerun production `0fea36df` / fixture `7272c87b` until the failure
          class has a fail-first regression and verified disposition
      - [x] Trace the failure to an ingress-epoch change during AT-SPI
            pre-dispatch work that is incorrectly classified as a generic
            protocol/backend failure; rule out a harness readiness workaround
      - [x] Introduce a typed, definitely-pre-dispatch ingress-conflict result
            across mutation, fresh observation, and targeted reconcile paths
        - [x] Classify every legitimate live preflight evidence-drift branch
              (action metadata, identity/state, text evidence, observation, and
              targeted refresh) as the same retryable conflict while keeping
              malformed, oversized, and unsupported protocol data terminal
      - [x] Prove one conflict is retried once with exactly one eventual
            dispatch for both protected `element_set_text` and semantic
            `text_insert`
      - [x] Prove repeated conflicts terminate boundedly as stale/no-effect,
            while generic protocol and every post-dispatch failure remain
            terminal and unreplayed
      - [x] Run focused AT-SPI/daemon tests plus complete workspace lint,
            documentation, and native gates before building another image
- [ ] Complete Phase 6 closure review, update implementation details, and commit
      and push the verified phase boundary

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
