# ADR-053: The web view is retired — serve, web/, /live and every HTTP route are deleted

**Status:** Accepted
**Date:** 2026-09-28
**Deciders:** George
**Supersedes:** [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md),
[ADR-046](ADR-046-the-web-ui-is-one-designed-system.md),
[ADR-048](ADR-048-the-operator-ui-is-a-typescript-spa-embedded-in-the-binary.md)

## Context

The web view lost its operator. The board carries stale web cards against a
surface George no longer opens, and `/kb-att` re-measures the queue it was
built to drain: approvals bottlenecked on the cost of looking at them
(ADR-016 Context: 75 open items across 6 of 13 boards, the oldest waiting 65
hours) are now answered from the terminal and the harness, not from
`kb.geoy.ws`. A served UI nobody opens is not a spare surface — it is an
unexercised write path (decide, undo, plan-open, subscription pause/resume),
an unaudited bundle pipeline, and a second client to keep in step with every
store change, all priced against zero visits.

The decision is therefore deletion, not replacement. There is no successor
surface to specify, no migration to stage, and no compatibility window to
honour: the operator's reads already have owners (below), and agents never
used the browser.

Work is tracked under epic `e-caeb1449` (ticket `t-fec3da9e`).

## Decision

`kanban serve`, the `web/` single-page application, the `/live` socket, and
every HTTP route — pages, the `/api/v1` JSON projection, and the four-verb
write allowlist — are deleted. No replacement UI is built. No compatibility
shim is left behind: no redirect, no stub route, no frozen bundle, no
`no-script` fallback. The specifications that described the surface,
`docs/specs/web-ui.md` and `docs/specs/spa.md`, are withdrawn and retained as
history.

## Consequences

- Unchanged, on the ledger's terms: the CLI, the generated MCP surface,
  `kb watch` / `kb events`, the deployment-attempt ledger and its
  served-commit provenance, the dashboard, and the search Ledger. Nothing in
  this decision renames, moves, or re-scopes any of them.
- Teardown row (operator-owned, same epic): `kb.geoy.ws` DNS, the SSO edge
  above it, and the `kanban-serve` systemd units go dark. The product holds
  no secret for any of them — TLS terminated at nginx, authentication lived
  at the edge — so there is nothing to rotate, only things to switch off.
- The write verbs the browser held do not move: decisions settle from the
  CLI, plans open from the CLI, subscriptions pause and resume from the CLI,
  each under the actor and audit rules that already governed those paths.
- History stays readable: ADR-016/046/048 are marked Superseded, never
  deleted, and the withdrawn specifications keep their requirement IDs so
  the matrix rows that cite them remain traceable to the words they meant.
