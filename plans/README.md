# Xenoteer planning corpus

Status: accepted implementation baseline
Last research pass: 2026-07-20
Target: first production-capable X11 release

## Purpose

This folder is the implementation contract for Xenoteer: a deterministic,
containerized Linux/X11 desktop that can be observed and controlled by a bot
with Puppeteer-like ergonomics. It is deliberately more prescriptive than a
typical roadmap. An implementation team should be able to select a phase,
follow its referenced subsystem plans, and build without inventing product or
architecture policy along the way.

Normative terms (`MUST`, `SHOULD`, `MAY`) carry their usual requirements
meaning. Defaults are API commitments only after the phase that introduces
them passes its exit gate. Before then they are implementation defaults subject
to measured adjustment.

## Reading order

| Order | Document | Question it answers |
|---:|---|---|
| 1 | [Product and decisions](00-product-and-decisions.md) | What are we building, and which choices are already closed? |
| 2 | [Phased implementation](15-phased-implementation.md) | In what order do we build it, and what proves each phase is done? |
| 3 | [Container runtime](01-container-runtime.md) | How does one container boot, supervise, expose readiness, and shut down? |
| 4 | [X11 desktop](02-x11-desktop.md) | How is the desktop made deterministic and accessibility-capable? |
| 5 | [Input engine](03-input-engine.md) | How are pointer, click, drag, scroll, and interpolation made reliable? |
| 6 | [Window management](04-window-management.md) | How are X11 windows identified, activated, moved, and verified? |
| 7 | [Keyboard and clipboard](05-keyboard-and-clipboard.md) | How are physical keys, Unicode, and X selections handled? |
| 8 | [Observation and streaming](06-observation-and-streaming.md) | How do screenshots, events, VNC, noVNC, and backpressure coexist? |
| 9 | [AT-SPI semantics](07-atspi-semantics.md) | How do semantic discovery, actions, waits, and fallbacks work? |
| 10 | [Rust architecture](08-rust-architecture.md) | What are the system boundaries, invariants, actors, and hard tradeoffs? |
| 11 | [Rust code design](09-rust-code-design.md) | What crates, modules, types, messages, and state machines get implemented? |
| 12 | [API and protocol](10-api-and-protocol.md) | What is the versioned wire contract and delivery behavior? |
| 13 | [SDK and CLI](11-sdk-and-cli.md) | What developer experience do Rust, TypeScript, Python, and the CLI provide? |
| 14 | [Security and isolation](12-security-and-isolation.md) | What is trusted, what is exposed, and what is denied by default? |
| 15 | [Testing and quality](13-testing-and-quality.md) | What tests, fixtures, performance budgets, and failure injection are required? |
| 16 | [Build, release, and operations](14-build-release-operations.md) | How are artifacts pinned, built, signed, operated, and upgraded? |
| 17 | [Dependencies and licensing](16-dependencies-and-licensing.md) | Which components are acceptable, and what redistribution obligations apply? |

## System map

```text
Rust / TypeScript / Python SDKs      Human observer
                 |                       |
                 v                       v
        HTTP + WebSocket API       noVNC (view-only)
                 |                       |
                 v                       v
        Action coordinator       websockify -> x11vnc
       /        |         \              |
      v         v          v             |
 Input actor  X11 observe  AT-SPI actor   |
      |         |          |              |
      +---------+----------+--------------+
                 |
          Xvfb + XFCE + D-Bus
```

The diagram hides an important rule: VNC is not a second control API. RFB input
would bypass Xenoteer's lease, ordering, deduplication, and pressed-input state.
The server is therefore view-only by default. Any future human-takeover feature
must revoke the bot lease before enabling human input and must make the mode
transition observable.

## Cross-document invariants

Every implementation and review must preserve these invariants:

1. One desktop has at most one physical-input action executing at a time.
2. Only the input actor owns the X connection used for XTEST input.
3. Observation cannot be blocked behind delayed XTEST requests.
4. All pressed keys and buttons are tracked and best-effort released after
   failure, cancellation, lease loss, or shutdown.
5. A command ID has one result within its deduplication retention window.
6. Window and element references include a desktop generation and can become
   explicitly stale; they are never silently retargeted.
7. Semantic invocation and physical clicking are distinct operations with
   distinct traces.
8. Readiness means the desktop is usable, not merely that PID 1 is alive.
9. VNC and X11 TCP listeners are not externally reachable by default.
10. Original Xenoteer server code follows the repository BSL; protocol schemas
    and SDKs intended for broad adoption are separately Apache-2.0.

## Configuration authority

Configuration has one direction of precedence:

```text
compiled safe defaults
  < image configuration file
  < mounted configuration file
  < explicitly supported environment variables
  < process arguments
```

Unknown keys and unknown environment variables with the `XENOTEER_` prefix are
startup errors. Secrets are accepted only by file path or secret provider
integration, never as command-line values. The daemon exposes its effective
non-secret configuration through the diagnostic API and startup log.

## Change discipline

- A code change that alters an invariant, public default, error category,
  reference lifetime, security boundary, or phase exit gate MUST update the
  relevant plan in the same pull request.
- A dependency replacement MUST update the dependency and licensing plan and
  preserve the relevant conformance tests.
- Deferred items do not become implicit requirements. Moving one into scope
  requires a recorded decision and a phase assignment.
- Research links are primary specifications or upstream documentation wherever
  possible. A link is evidence for a constraint, not permission to delegate a
  product decision to upstream behavior.

## Definition of implementation-ready

A phase is ready for a team only when it names:

- inputs, outputs, ownership, and dependency order;
- public and internal types it introduces;
- failure and cancellation behavior;
- defaults and configuration knobs;
- tests and measurable exit criteria;
- security and license effects;
- explicitly deferred work.

The phase plan and linked subsystem plans satisfy that standard. If an edge
case is genuinely absent, the implementer should add it to the appropriate plan
before selecting an ad hoc behavior in code.
