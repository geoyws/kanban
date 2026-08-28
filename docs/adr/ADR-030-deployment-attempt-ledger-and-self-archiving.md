# ADR-030: Deployment attempts are append-only and self-archive from hot indexes

**Status:** Accepted
**Date:** 2026-08-28
**Deciders:** George, Kanban maintainers

## Context

Kanban records the work that leads to a release, but it cannot currently answer which exact pushed commit was attempted on a tier, whether the live service was verified at that commit, or what currently serves each environment. Deployment claims consequently live in transient chat, scripts and CI output instead of the audit journal.

The history must remain useful over years. Keeping every old attempt in default listings and hot secondary indexes would steadily make the operational view noisier and more expensive, while deleting attempts would break auditability.

## Decision

### Attempt ledger and identity

Each board owns an append-only deployment-attempt ledger. An attempt has a stable `d-*` identifier and records an optional task, canonical repository identity, the mandatory full 40-character pushed commit, source branch, canonical tier, environment or release slug, host, URL, actor, lane, timestamps, mechanism, optional artifact URI, optional retry predecessor and optional caller-supplied idempotency key.

Canonical tiers are exactly `@_bdt`, `@_bd`, `@_bst`, `@_bs`, `@_s`, `@_uat` and `@_p`. The environment key is `(repo, tier, environment)`. A retry is a new attempt linked with `retryOf`; history is never rewritten into a retry.

### State and verification

An attempt starts in `started` and reaches exactly one terminal result: `succeeded`, `failed`, `cancelled` or `abandoned`. Finish requires the capability token returned by start. Explicit stale recovery may abandon an attempt with an audited force override; spelling an actor as `geo` is not authorization.

`succeeded` means live verification completed, not merely that a command exited zero. It requires a non-empty receipt and an observed served commit equal to the requested commit. The recorded phase distinguishes build, publish, start and verification failures. `abandoned` is distinct from `failed` because no deployment failure was observed.

The current release for an environment is derived from its newest successful verified attempt. There is no hand-writable current-release table that can diverge from the immutable ledger.

### Self-archiving

The normal archive sweep also archives terminal deployment attempts older than its cutoff when they are not the current successful attempt for their environment. A `started` attempt and the latest successful attempt for every `(repo, tier, environment)` always remain hot, regardless of age.

Archiving is a projection change, not deletion. Archived attempts remain in the board database, hash-chained audit trail, backups and explicit `--all` queries and searches. Default lists, current projections and hot partial indexes exclude them. Re-running a sweep is idempotent. Production schedules the existing archive sweep with a 90-day cutoff, so maintenance is automatic rather than dependent on an agent remembering a special deployment cleanup command.

### Interfaces

The compiled CLI and generated MCP surface provide `deploy start`, `finish`, `abandon`, `show`, `list` and `current`. The web service provides a cross-board current-release matrix, active and recent failed attempts, per-attempt detail and live refresh through the existing WebSocket revision channel.

## Consequences

- A release claim can cite an immutable attempt and exact served-commit receipt.
- Failed and abandoned attempts remain evidence instead of being overwritten by a later success.
- Operational indexes stay bounded over years without weakening recovery or audit history.
- Board-local storage preserves project ownership; the web service performs cross-board aggregation.
- Callers must retain the short-lived capability token until the attempt reaches a terminal state.
- Historical deployments that predate this schema are not invented or backfilled without evidence.

## Evidence required

Compiled-process tests must prove the state machine, token enforcement, idempotency, exact served-commit success gate, current projection, search visibility, and archive behavior. The archive test must show that old terminal non-current attempts leave default and hot-index views, while started and current successful attempts stay hot and every archived attempt remains available through `--all` and audit history.

Deployment of this feature is complete only after the exact pushed production commit is itself recorded, finished with a matching served commit, and visible in the live web projection.

## References

- [ADR-006: Rust runtime and compiled binary E2E](ADR-006-rust-runtime-and-compiled-binary-e2e.md)
- [ADR-008: Web server safety boundary](ADR-008-web-server-safety-boundary.md)
- [ADR-010: MCP parity](ADR-010-mcp-parity.md)
- [ADR-016: Search is a derived index](ADR-016-search-is-a-derived-index.md)
- [ADR-021: Settled history leaves operational indexes](ADR-021-settled-history-leaves-operational-indexes.md)
- [ADR-029: Audit journals are hash-chained and externally anchored](ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md)
- Epic `e-a89b3db2`: Deployment tracking and current release matrix
