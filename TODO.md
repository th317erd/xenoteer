# Xenoteer implementation TODO

This checklist tracks execution of the normative plan in
`plans/15-phased-implementation.md`. Tests and review gates are part of every
item, not cleanup work after implementation.

## Completed

- [x] Phase 0: prove the technical baseline and production container lifecycle.
- [x] Phase 1: implement deterministic XTEST pointer and keyboard actors.
- [x] Phase 2: build the deterministic XFCE desktop and supervised service graph.
- [x] Phase 3: implement the authenticated raw control plane and coordinator.

## Phase 4: observation and desktop primitives

- [x] Define strict protocol contracts for windows, selectors, clipboard,
  capture, artifacts, viewer tickets, and window-control evidence.
- [x] Add centralized grants and command authorization requirements.
- [x] Add the private, immutable, scoped artifact-store core.
- [x] Finish and review authenticated artifact HTTP upload/read/delete routes.
- [x] Add single-range artifact downloads with bounded streaming, 416 handling,
  integrity headers, `nosniff`, disposition, and private no-store policy.
- [x] Add generation-fenced live-window identities and bounded tombstones.
- [x] Bound per-XID birth-history memory for the full desktop lifetime without
  permitting an old serialized reference to retarget after tombstone expiry.
- [x] Finish and review the window selector/query/resolve/wait engine, including
  explicit creation revisions.
- [x] Add bounded X11 atom/property/inventory/geometry/event primitives.
- [x] Add and review the single-owner X11 observation actor, checked event
  subscriptions, loss signaling, and bounded focus-ancestry evidence.
- [x] Implement the daemon artifact service adapter with bounded streaming,
  integrity verification, scoped access, and cancellation cleanup.
- [x] Add the daemon-internal artifact publication/consumption boundary for
  generated screenshots, clipboard output, and exact clipboard-input rechecks.
- [x] Wire the daemon artifact adapter through configuration/readiness and add
  an explicit bounded upload deadline/idle timeout.
- [x] Integrate the daemon-owned observation service adapter and live model feed.
- [x] Prove loss-triggered resynchronization remints every live window identity,
  so an XID destroyed and recreated behind event loss cannot retarget an old
  reference even when its metadata is identical.
- [x] Add bounded, authenticated reference-token and pagination-cursor codecs,
  including exact 32-byte secrets, collision retry, digest-only storage,
  deterministic test seams, and principal/query/order/revision/expiry binding.
- [x] Prove observation waits use atomic check-register-recheck and cannot miss
  a model transition or leak admission slots on timeout/cancellation.
- [x] Implement and verify EWMH window-control effects with exact revalidation.
- [x] Add the missing public move-to-workspace command and integrate every
  window-control command through daemon effect execution and observed results.
- [x] Correlate observed windows to managed process references with explicit
  evidence/confidence/conflict handling; `_NET_WM_PID` alone remains low trust.
- [x] Emit normalized window lifecycle/metadata/focus/geometry/rebuild events
  only after committed model transitions through nonblocking coordinator ingress.
- [x] Implement clipboard actors, INCR transfer handling, and exact-text fallback.
- [x] Expose bounded authenticated clipboard read/set/clear/paste APIs and wire
  artifact-backed payloads plus InputActor paste coordination.
- [x] Implement the bounded raw screenshot actor, visible/drawable semantics,
  cursor composition, encoding, and exact near-effect window revalidation.
- [x] Add bounded MIT-SHM 1.2 capture with core-GetImage fallback metrics and
  prove root/window/cursor/occlusion parity on an isolated live Xvfb display.
- [x] Implement root X DAMAGE subscription, 16 ms bounded coalescing, public
  `screen.damaged` hints, and overflow-to-resync coordinator delivery.
- [x] Integrate screenshot capture with private artifact persistence.
- [x] Expose bounded authenticated screenshot routes/commands; the protocol DTO
  and artifact purpose alone do not make capture reachable.
- [x] Implement the bounded one-use, principal/origin/generation-bound viewer
  ticket registry and authenticated issuance route.
- [x] Implement the view-only viewer gateway and prove it cannot inject input.
- [x] Stabilize recursively strict request/additive response compatibility and
  regenerate all schemas with direction-aware regression gates.
- [x] Expose the complete atomic xdotool-style input surface and strict
  window-relative click with owner-thread identity/focus revalidation.
- [x] Finish frame/client move-resize bounds policies and quiet-window
  convergence instead of rejecting declared protocol policies.
- [x] Make runtime capabilities reflect live per-operation backend evidence.
- [x] Bring OpenAPI/public route documentation up to the implemented Phase-4 API.
- [x] Run the GTK/Qt/browser/XFCE live clipboard, window, and capture fixture
  matrix; do not substitute fake/unit coverage for these gates.
- [ ] Re-enable exact clipboard insertion for QtWebEngine after upstream Qt
  fixes its X11 accessibility-path duplicate-paste defect or after Xenoteer has
  a truthful toolkit-specific DOM adapter. Keep this isolated exception from
  weakening exact postconditions for GTK3, Qt6, Chromium, or Firefox.
- [x] Run targeted, workspace, live-X11, image, security, clippy, rustdoc,
  formatting, license, and adversarial review gates with explicit timeouts.
- [x] Commit the verified Phase 4 boundary locally.

## Phase 5: accessibility semantics and semantic actions

- [x] Define strict ElementRef, snapshot, selector, query, wait, event, action,
  correlation, and capability contracts with protected-field redaction rules.
- [x] Add pure bounded accessibility identity/cache/query/wait models, including
  deterministic traversal, cycles, malformed parents, ambiguity, and budgets.
- [x] Implement the single-owner AT-SPI actor with independent degraded state,
  generation fencing, bounded reconnect, explicit shutdown, and stream failure.
- [x] Implement bounded Cache `GetItems`, incremental updates, lazy traversal,
  old-Qt compatibility decoding, reconciliation, and resync-on-gap behavior.
- [x] Integrate daemon accessibility state, authenticated read APIs, pagination,
  snapshots, race-free waits, normalized events, and live capability evidence.
- [x] Correlate applications/elements with process and window evidence without
  authorizing physical effects from weak title/PID evidence alone.
- [x] Implement cancellable semantic invoke/focus/value/selection/text/scroll
  actions with exact reference revalidation and bounded postconditions.
- [x] Complete `text.insert auto`, protected-field policy, and explicit strategy
  evidence without silently substituting semantic and physical operations.
- [x] Implement physical `element.click` with activation, scroll, geometry,
  occlusion, queue-delay revalidation, and interpolated InputActor execution.
- [x] Pass full workspace all-feature/all-target tests, Clippy, Rustdoc, schema,
  API-documentation, container-static, dependency, audit, license, and native
  gates for the Phase 5 source.
- [x] Pass the bounded diagnostic live gate against the current daemon for
  GTK3/Qt6 restart fencing, Chromium reload fencing, semantic/physical effects,
  the 4,096-node materialized surface, depth-budget rejection, oversized-name
  isolation, bus reconnect, and a 5,000-mutation event flood.
- [x] Build the coherent exact production and desktop-app fixture images, record
  their immutable IDs, rerun the 25-minute Phase 5 live/image gate with no
  daemon override, finish the closure review, and commit the Phase 5 boundary.

## Later phases

- [ ] Phase 6: SDKs, CLI, recording/replay, compatibility, and operability.
- [ ] Phase 7: release hardening, reproducibility, documentation, and final
  gates, including 10,000-node cold-snapshot timing, selector p95, event-lag,
  cache-RSS, and large-browser soak measurements.

## Resource policy

- [x] Keep Rust work at `--jobs 2` and `RUST_TEST_THREADS=2` or lower.
- [x] Run heavy work with `nice -n 15 ionice -c 3` and a sane timeout.
