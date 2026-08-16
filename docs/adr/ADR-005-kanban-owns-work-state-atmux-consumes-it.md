# ADR-005: Kanban owns work state and atmux consumes it

**Status:** Accepted
**Date:** 2026-08-14
**Deciders:** George

## Context

atmux currently owns a mature Kanban implementation spanning task, story, and
epic schemas; SQLite repositories and migrations; claims; dependencies; lanes;
assignment; dispatch; dashboards; hygiene checks; and handoff/rotation flows.
This repository already owns a second SQLite ledger with stronger continuity
checkpoints and portable agent-facing APIs. Keeping both creates competing
sources of truth and prevents projects outside atmux from using the same board.

## Decision

The `geoyws/kanban` repository becomes the only implementation and source of
truth for personal work state. atmux remains responsible for tmux sessions,
worktrees, agent processes, routing, and cockpit presentation, but consumes
Kanban through its typed package/CLI interface.

The change is a parity-gated migration:

1. inventory atmux's task, story, epic, claim, dependency, lane, assignment,
   event, dashboard, hygiene, and handoff behavior;
2. implement equivalent domain operations and compatibility fields in Kanban;
3. add an atmux adapter that points each team/project and its worktrees at the
   operator-private Kanban registry and board;
4. migrate existing `.atmux/state.db` and legacy `kanban.json` work state with
   stable IDs, timestamps, statuses, relationships, notes, and extra fields;
5. run old/new parity tests and produce row-count and field-level migration
   receipts;
6. switch atmux readers and writers to the Kanban adapter; and
7. only after a rollback-capable observation period, remove atmux's duplicate
   Kanban schemas, repositories, migrations, JSON writers, and handoff files.

The migration never deletes the rest of atmux's SQLite state. Existing state
databases and JSON are archived or left read-only until verification and
rollback gates pass. Kanban state remains operator-private and outside the
atmux or product repository.

## Consequences

All projects and harnesses share one portable work-state contract, while atmux
focuses on process and workspace orchestration. Handoffs survive tmux pane and
harness replacement through the same Kanban context packet.

The takeover is larger than replacing a table: many atmux behaviors are coupled
to inboxes, lane routing, epics, stories, events, cron, and cockpit views. The
duplicate implementation cannot be removed safely until those consumers are
adapted and parity receipts pass. During migration, compatibility adapters are
required and both codebases must be changed in a controlled sequence.

## References

- [ADR-003](ADR-003-private-multi-project-personal-work-system.md)
- [ADR-004](ADR-004-token-pressure-handoffs-through-kanban.md)
- [atmux integration plan](../integrating-atmux.md)
- [Product requirements](../PRD.md)
