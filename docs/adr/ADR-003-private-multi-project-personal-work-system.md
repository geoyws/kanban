# ADR-003: Use Kanban as the private multi-project personal work system

**Status:** Accepted
**Date:** 2026-08-14
**Deciders:** George

## Context

The operator works across multiple projects and multiple Git worktrees. Tasks,
progress, evidence, and restart instructions currently risk being scattered
across repository-local TODO and handoff files, chat history, and harness state.
Those locations do not provide one reliable personal view and can diverge when
several agents or worktrees update progress concurrently.

This system is for the operator's own coordination. It must not become product
state, team workflow, or a file committed into repositories maintained with
other developers.

## Decision

Kanban is the operator-private source of truth for active projects, tasks,
progress, evidence, checkpoints, and handoffs.

- SQLite remains authoritative and lives below the operator's private Kanban
  data directory, outside managed repositories.
- Each project has one board database. All registered worktrees for that
  project resolve to the same board, so they see and mutate the same state.
- The private registry maps project and worktree roots to board databases and
  supplies an aggregate personal dashboard across registered projects.
- Mutations use typed operations and transactions. Callers do not write SQL.
- Markdown TODO or handoff files may be generated as disposable projections,
  but are never authoritative and are not required for continuity.
- Project registration is explicit. Kanban does not crawl repositories or
  publish, synchronize, or expose board data to other developers by default.

The first delivery slice adds worktree attachment, an aggregate project view,
and explicit agent handoffs. A user interface and cross-host synchronization
remain later projections over the same contract.

## Consequences

Every registered worktree can query and update the same project plan without
committing private coordination files. The operator can inspect all projects
from one registry while retaining a project-scoped corruption and backup
boundary.

The registry becomes important private state and needs eventual backup and
retention support. Moving or deleting worktrees requires alias maintenance.
SQLite provides atomic local multi-process coordination, not automatic
cross-host replication.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md)
- [Product requirements](../PRD.md)
