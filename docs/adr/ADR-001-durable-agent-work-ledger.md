# ADR-001: Kanban is a durable agent-work ledger

**Status:** Accepted
**Date:** 2026-08-11
**Owner:** geoyws

## Context

Long-horizon work fails when plan and progress exist only in a model's context.
An agent may be replaced because of token pressure, rate limits, provider
failure, process restart, or deliberate swarm scheduling. Conversation history
is therefore a cache, not a durable coordination boundary.

atmux already proved the local SQLite approach for multi-writer task state. Its
accepted SQLite decision and proposed durable-continuity work provide the seed,
but the substrate should not belong to a tmux-specific product. opencode orch,
atmux, and future harnesses need one small contract.

## Decision

### 1. SQLite is authoritative; Markdown is a projection

Machine-managed task state lives in SQLite. Generated `TODO.md` views help
humans and simple agents restart, but editing them never mutates state. This
prevents two sources of truth.

SQLite uses WAL, `synchronous=NORMAL`, foreign keys, a five-second busy timeout,
prepared statements, and immediate transactions for ownership decisions.

### 2. State is local and federated

Each workspace has an isolated board database. An operator-private registry
maps workspace roots to boards and enables aggregate tooling. State lives below
the XDG data directory by default, outside managed repositories.

This is preferable to one host-wide database because it limits privacy and
corruption blast radius, keeps backup/retention policy project-scoped, and lets
atmux retain per-team storage. Cross-workspace views query the registry.

### 3. Claims are leases, not permanent ownership labels

A claim is atomic and carries an opaque token, agent identity, optional session
identity, heartbeat time, and expiry. Only the current token can heartbeat,
checkpoint, release, block, or complete the task. Expired work becomes
claimable without deleting its history.

This provides swarm exclusion while allowing recovery from dead agents.

### 4. Narrative state is append-only

Plans, progress, blockers, decisions, evidence, and completion notes are rows,
not rewrites of the task specification. Corrections append new notes. This
avoids concurrent read-modify-write loss and preserves why decisions changed.

### 5. Checkpoints are structured cold-start contracts

Every meaningful work turn should checkpoint:

- summary of work performed;
- current intent and next concrete action;
- blockers and validation evidence;
- agent, session, and model identity;
- repository path, branch, HEAD, and dirty-worktree summary;
- `continue`, `blocked`, or `done` state.

A context packet always retains task identity/specification, dependency state,
current claim, and the newest checkpoint. Older history is discarded first
when rendering a bounded model prompt. Durable storage is never truncated.

### 6. Consumers receive narrow operations, not arbitrary write SQL

The TypeScript API and CLI own validation and transitions. Future MCP and
plugin adapters expose the same operations. Read-only diagnostic SQL may be
added later, but agents do not receive unrestricted mutation access.

### 7. Integration is incremental

Kanban does not replace atmux state in one migration:

1. establish Kanban schema, API, CLI, and contract tests;
2. add orch long-horizon turns backed by Kanban;
3. add an atmux adapter preserving existing commands and data;
4. migrate only after parity receipts prove task and continuity semantics;
5. add UI or remote synchronization as optional projections/transports.

## Consequences

Fresh agents can resume without pane scrollback or conversation history. Swarm
workers coordinate through atomic claims and durable evidence. Product repos
remain clean, and runtime-specific integrations remain thin.

The design introduces lease handling, schema migration discipline, retention
needs for append-only history, and an eventual compatibility migration for
atmux. SQLite coordinates processes on one host; cross-host synchronization is
explicitly deferred and must not be implied by the v0.1 durability claim.
