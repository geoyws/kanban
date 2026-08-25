# ADR-022: Roadmap todo lists are child-epic trees

**Status:** Accepted
**Date:** 2026-08-25
**Deciders:** George

## Context

Agents need durable todo lists for work that spans more than one action. A
Markdown checklist is easy to write, but it becomes a second status store beside
Kanban: an item can be checked in one place and open in the other, its work can
be claimed without updating the list, and the list disappears when it lived
only in an agent's context or scratch file.

[ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md) established
that an epic's body is a plan and its children are the work produced by that
plan. A roadmap is the recurring case where the operator also wants to see each
major item become complete without maintaining a duplicate checklist.

## Decision

Every durable multi-item todo list is represented by a Kanban roadmap tree:

- the roadmap is an epic;
- every top-level todo item is a direct child epic;
- stories and tasks beneath that child epic are the actionable work;
- dependencies between child epics express ordering; and
- the board tree and its statuses are authoritative.

The roadmap body records scope, rationale, ordering and success criteria. It
does not contain a manually maintained checkbox copy of the child epics. Any
Markdown checklist, dashboard or progress count is a projection of the board,
never a second writable source of truth.

Use `draft` while shaping the roadmap and its children, then move the roadmap to
`todo` when the tree is ready to act on. A draft ancestor holds back its entire
tree as established by ADR-013.

A direct child epic is the roadmap item's checkbox. Until Kanban implements a
generic derived epic-completion gate, an agent may mark that child epic `done`
only after every non-cancelled descendant is settled and the relevant evidence
is durable in checkpoints or events. This is an explicit agent-verified
transition, not an automatic property of the current CLI.

A single standalone action remains a task. Agents must not create ornamental
roadmap and child epics around one item merely to satisfy this convention.

## Consequences

Roadmap progress survives model turns, worktree recreation and agent changes.
Claiming, leases, checkpoints and completion all operate on the same tree that
the operator reads, so there is no checkbox file to reconcile.

The direct-child rule preserves a stable level for roadmap reporting even when
each item later grows its own sub-epics. It also gives future CLI and web views a
deterministic projection: count or render the roadmap epic's direct child epics.

Completion is presently procedural. The CLI does not yet prove that every
descendant is settled when an epic is moved to `done`; documentation and global
agent instructions must say that plainly. A future derived transition may
automate the check without changing the data model or this decision.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — the authoritative work ledger
- [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md) — nested plans and draft-tree gating

