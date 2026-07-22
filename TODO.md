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
- [x] Run targeted, workspace, live-X11, image, security, clippy, rustdoc,
  formatting, license, and adversarial review gates with explicit timeouts.
- [x] Commit the verified Phase 4 boundary locally.

## Later phases

- [ ] Phase 5: accessibility semantics and semantic actions.
- [ ] Phase 6: SDKs, CLI, recording/replay, compatibility, and operability.
- [ ] Phase 7: release hardening, reproducibility, documentation, and final gates.

## Resource policy

- [x] Keep Rust work at `--jobs 4` and `RUST_TEST_THREADS=4` or lower.
- [x] Run heavy work with `nice -n 15 ionice -c 3` and a sane timeout.
