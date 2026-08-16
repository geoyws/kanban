# Kanban product requirements

**Status:** Active
**Owner:** George
**Updated:** 2026-08-16

## Product statement

Kanban is George's private, stateful representation of the projects he is
working on. It gives humans and compatible agent harnesses one durable place to
discover work, coordinate atomic ownership, report progress, preserve evidence,
and transfer work to a replacement agent when context or tokens run low.

## Goals

1. Show active work and progress across multiple registered projects.
2. Let every worktree of a project share the same project board.
3. Let agents safely read and update tasks through narrow typed operations.
4. Make token-pressure handoffs cold-startable without chat history or
   `HANDOFF.md`.
5. Keep all operational state private to the operator and outside product
   repositories.
6. Preserve history and prevent concurrent ownership or partial transitions.
7. Replace atmux's embedded Kanban implementation and make atmux a consumer of
   this portable work-state engine.

## Non-goals

- A shared issue tracker for coworkers, customers, or delivery teams.
- Committing the live board, TODO files, or handoff files into product repos.
- Arbitrary SQL access for agents.
- Cross-host replication in the first release.
- Replacing Git, source documentation, or external customer issue systems.

## Primary workflows

### Register a project and its worktrees

The operator initializes one canonical project root. Additional worktree roots
attach to that project and resolve to the same board. Registration is explicit
and reversible; no filesystem crawl is required.

### Plan and execute work

An agent lists project tasks or the personal cross-project view, claims eligible
work with an expiring lease, appends progress/evidence, and checkpoints after
meaningful turns. Dependencies and leases prevent unsafe parallel execution.

### Hand off under token pressure

Before an agent runs out of useful context, it creates a `token_pressure`
handoff with a summary, current intent, one concrete next action, blockers,
validations, and repository state. Kanban atomically persists that packet and
releases ownership. A replacement agent accepts the handoff, receives a new
lease, loads the context packet, and continues.

### Review the personal portfolio

The operator can request an aggregate view containing each registered project,
its worktree roots, active task counts, blocked work, pending handoffs, and
recent progress. Reads occur across project databases without merging their
write domains.

## Functional requirements

### P0 — first usable slice

- One SQLite board per project and one private SQLite registry.
- WAL mode, foreign keys, busy timeout, append-only migrations, and atomic
  ownership transitions.
- Attach multiple worktree roots to the same project board.
- Aggregate registered projects and task/handoff counts.
- Create, list, and accept structured agent handoffs.
- Include handoffs in cold-start context.
- Preserve the existing task, dependency, claim, note, checkpoint, and TODO
  projection contracts.
- Rust-only production CLI and domain engine, with Cargo-based development and
  compiled-binary end-to-end tests.

### P1

- atmux parity for task assignment, lanes, dependency updates, driver-only
  gates, stories, epics, readiness, sign-off, dispatch/inbox integration,
  hygiene checks, and cockpit projections.
- Importer for atmux `state.db` and legacy `kanban.json`, preserving stable
  identities, relationships, timestamps, notes, and extension fields, with an
  explicit atomic reconcile mode for stopped-writer cutover refreshes.
- Compatibility adapter followed by the verified removal of duplicate atmux
  Kanban storage and repository code.
- Operator-oriented terminal or web board over the same APIs.
- Search/filter by project, worktree, status, priority, assignee, and recency.
- Safe project/worktree detach and rename operations.
- Backup, restore, integrity-check, and retention commands.
- Harness adapters that automatically checkpoint and initiate handoff before a
  configured token threshold.

### P2

- Optional encrypted synchronization between the operator's own hosts.
- Notifications for stale claims, blocked work, and unaccepted handoffs.

## Handoff acceptance criteria

A handoff is valid only when:

- the outgoing agent owns a live lease for the task;
- summary, intent, and next action are non-empty;
- its checkpoint and lease release commit in the same transaction;
- the pending record is visible in task context;
- acceptance creates a different live lease and records the incoming agent;
- a named target cannot be accepted by a different agent; and
- a stale outgoing token cannot mutate the task after handoff.

## Privacy and safety requirements

- Default storage is `${XDG_DATA_HOME:-~/.local/share}/kanban`.
- Live state is ignored by and physically separate from managed repositories.
- No network listener, telemetry, publication, or remote synchronization is
  enabled by default.
- Secrets and lease tokens do not appear in notes, checkpoints, handoffs, or
  rendered model context.
- Every mutation has an actor and an auditable event.

## Success measures

- A task created from one worktree is immediately visible from another attached
  worktree.
- Two processes cannot simultaneously own the same task.
- An outgoing and incoming agent can complete a handoff using only Kanban and
  the repository checkout.
- The aggregate view accurately reports all explicitly registered projects.
- Process restart and database reopen preserve all task and handoff state.

## Delivery slices

1. **Foundation (current):** worktree aliases, aggregate project summary,
   transactional handoffs, CLI/API, context, and tests.
2. **atmux parity:** port its mature task/story/epic behavior and build import
   plus compatibility contracts.
3. **atmux cutover:** migrate state, switch every consumer, verify receipts,
   and remove the duplicate implementation with rollback retained.
4. **Operational hardening:** integrity/backup commands and automatic harness
   token-threshold integration.
5. **Personal UI:** fast portfolio and project board views.
6. **Optional mobility:** encrypted operator-only cross-host transport.

## Current delivery status

The foundation and most atmux parity surfaces are implemented. Seven non-empty
legacy project sources have been imported into healthy private boards without
activation, producing a 3,042-row fleet preparation receipt. The aggregate
dashboard is usable now. Authority cutover remains gated on durable Kanban
handoffs for live agents, stopped writers, per-project activation receipts,
consumer restart verification, and a no-legacy-write observation period.

The production runtime is Rust per ADR-006. The compiled executable passes the
full process-boundary E2E gate, opens read-consistent snapshots of all eight
existing private boards (3,046 tasks), and satisfies atmux's focused CLI adapter
contract suite. TypeScript and Bun are not production or development
dependencies of Kanban.

See [the 2026-08-16 fleet preparation receipt](migrations/atmux-fleet-preparation-2026-08-16.md).
