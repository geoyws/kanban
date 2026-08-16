# ADR-006: Use Rust for the Kanban runtime and require compiled-binary E2E

**Status:** Accepted
**Date:** 2026-08-16
**Deciders:** George

## Context

Kanban is the durable authority for private multi-project work and agent
handoffs. Its first implementation used TypeScript executed by Bun. The
operator requires Kanban itself to be written in Rust and requires end-to-end
proof that the installed executable—not an in-process store or source runner—
survives real process boundaries and concurrent access.

The existing private registry and project boards already contain live SQLite
state. A language rewrite must therefore preserve the released database schema,
migration ladder, JSON field names, CLI contract, privacy modes, and atomic
handoff semantics. Requiring users to export and re-import their board merely
to change implementation language would introduce avoidable data risk.

## Decision

Rust is the only production runtime for Kanban. The `kanban` executable is a
compiled Cargo binary. TypeScript was retained temporarily as a migration
oracle while parity was proven, then removed after the Rust E2E and atmux
adapter gates passed.

The Rust implementation opens the existing registry and board databases in
place. It retains SQLite WAL mode, foreign keys, busy timeout, append-only
migrations, mode `0700` data directories, mode `0600` databases/backups, and
`BEGIN IMMEDIATE` ownership transitions.

Release qualification requires E2E tests that spawn the compiled binary as
separate processes and exercise at least:

1. initialization, process restart, task persistence, notes, and context;
2. canonical workspace plus attached worktree sharing one board;
3. two concurrent claimers with exactly one winner;
4. checkpoint plus token-pressure handoff plus acceptance with lease rotation;
5. stale outgoing-token rejection and absence of lease tokens from read models;
6. atmux SQLite and JSON import through the CLI;
7. dashboard, doctor, backup, and reopen of the resulting snapshot; and
8. direct compatibility against a copy of the current private database schema.

Unit tests remain useful but cannot substitute for this compiled-binary E2E
gate. A test that invokes library functions in-process is not described as E2E.

## Consequences

Kanban becomes a single native executable with no Bun runtime dependency in
production. Rust's ownership and error handling strengthen the boundary around
leases and transactional state, while the existing SQLite format avoids a
second migration event during the atmux cutover.

Cargo dependencies and `Cargo.lock` are authoritative. The release gate passed
against the compiled binary, snapshots of the existing SQLite v3 fleet, and the
atmux CLI adapter; `bun.lock`, the TypeScript sources, and their package metadata
were then removed.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md)
- [ADR-004](ADR-004-token-pressure-handoffs-through-kanban.md)
- [ADR-005](ADR-005-kanban-owns-work-state-atmux-consumes-it.md)
- [Product requirements](../PRD.md)
