# Xenoteer implementation plan

Status: accepted for implementation
Planning baseline: 2026-07-20

Xenoteer's implementation plan is maintained as a planning corpus under
[`plans/`](plans/README.md). The corpus is the normative implementation guide;
this file is only its stable entry point.

Start with:

1. [`plans/README.md`](plans/README.md) for scope, reading order, and document
   ownership.
2. [`plans/00-product-and-decisions.md`](plans/00-product-and-decisions.md) for
   accepted product and architecture decisions.
3. [`plans/15-phased-implementation.md`](plans/15-phased-implementation.md) for
   the dependency-ordered delivery plan and phase exit gates.

The detailed Rust design is split intentionally:

- [`plans/08-rust-architecture.md`](plans/08-rust-architecture.md) defines the
  high-level architecture, invariants, concurrency model, and critical design
  analysis.
- [`plans/09-rust-code-design.md`](plans/09-rust-code-design.md) defines the
  workspace, crates, modules, types, actors, state machines, and code-level
  contracts.

When this entry point and a subsystem plan disagree, the subsystem plan wins.
When two subsystem plans disagree, stop implementation and resolve the conflict
in `plans/00-product-and-decisions.md` before changing code.
