# ADR-013: A plan is an epic, and a draft is not yet work

**Status:** Accepted
**Date:** 2026-08-21
**Deciders:** George

## Context

Two states of work had nowhere to live, and both were being kept outside the
ledger as a result.

**A plan.** Everything the board held described work that already exists: a task,
its progress, who holds it, what was handed over. There was nothing for the thing
that comes first — how a piece of work is intended to go, written before it is
decomposed, often spanning several epics or none yet. That lived in a markdown
file beside the repo, which is the same failure the session handoff had before
[ADR-012](ADR-012-session-handoffs-and-durable-attention.md): keyed to a
directory, orphaned the moment a lane moves, and invisible to anyone who does not
already know it is there.

**A row that is not finished being written.** `backlog` already meant real work
that is simply unscheduled. There was no state before that. Agents read every row
on the board as a specification, so a half-written one gets decomposed, depended
on and worked as though it were settled, and the ledger said nothing to stop it.

## Decision

### A plan is an epic

There is no plan entity. An epic's **body** is the plan, its **children** are the
work it became, and `--parent` answers "what did this plan produce" — better than
a link table, because it is the same relation the breakdown already uses and
nobody has to maintain it separately.

A `plans` table with its own supersession chain was designed and then discarded.
It duplicated the container that already existed, and it would have needed a
link mechanism to connect a plan to its work — reinventing `--parent` beside
`--parent`.

**A plan is not an ADR.** An ADR records a decision and why it was taken; a plan
records intended work. ADRs stay in `docs/adr`.

Three things had to become true for an epic to actually carry a plan:

- **Plans nest.** The rule in
  [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
  shipped as "a child must be strictly narrower than its parent", which refused
  epic-under-epic. A programme plan must hold its sub-plans, so the rule is now
  stated as containment — an epic holds epics, stories and tasks; a story holds
  tasks; a task holds nothing — rather than computed from a depth. Depth
  arithmetic could only express the epic case as an exception bolted onto a rule
  it contradicts. Story-under-story, task-under-task, and any container inside
  something narrower stay refused.
- **A revision keeps what it replaced.** `task_updated` wrote an empty payload:
  an audit trail recording *that* something changed and nothing about *what*. For
  most fields that is merely thin, since the current value sits on the row. For a
  body it is destructive — revising a plan replaced it with no record the
  previous version existed. The event now names every field that moved and
  carries the previous body when the body moved. Only the body, because
  everything else is readable off the row and a replaced body is not. `events
  --task <epic>` is therefore a plan's revision history, by construction rather
  than by anyone remembering to keep a copy.
- **A plan can be loaded from a file.** `--body-file` on `task add` and
  `task update`, because a plan is markdown measured in kilobytes and a command
  line is a miserable place to put it. Passing `--body` and `--body-file`
  together is refused rather than ranked, exactly as the board selectors are:
  two answers to one question, only one is what the caller meant, and nothing in
  the receipt would say which was stored.

### A draft is not yet work

`draft` is a task status, and it is the state before `backlog`: a row still being
written, whose title, body or scope may yet be wrong.

It needed no new rule to be safe. `claim` already accepts only `todo` and
`in_progress`, and `--next` selects `todo` alone, so a draft is skipped however
urgent its priority and refused when named explicitly. `task move <id> todo`
promotes it. It is otherwise a first-class status: filterable, counted on the
dashboard, and the status set stays closed so an invented one is still refused.

The two decisions are one design. **A plan saved up but not ready to act on is a
draft epic** — which is what "projects can have plans saved up" asks for, without
either feature needing to know about the other.

### A draft holds back the work beneath it

**Amended 2026-08-22.** `draft` first shipped protecting only the row it sat on.
Since a plan is an epic, drafting a plan and hanging work under it produced tasks
that were immediately claimable: `claim --next` handed a driver work from a plan
nobody had opened, and naming that task explicitly worked too. Whether the plan
was ready was recorded on the plan and consulted by no one.

Nothing under a draft is offered or granted. The walk goes to the top of the
parent chain rather than checking the immediate parent, because a plan holds
sub-plans and the drafted thing is usually two levels up. `--next` steps over the
whole drafted tree; an explicit claim is refused naming the draft ancestor and
the command that opens it, since an agent told only "no" has no next move. All
three paths that mint a lease share one implementation — `claim` by id,
`claim --next`, and `handoff accept` — so they cannot refuse for different
reasons.

This is what makes the driver model work as stated: **drafts are visible to
every driver, and pickable by none until opened.** A draft is hidden from the
queue, not from the reader. Nothing else about drivers needed changing — they
were already just identities passed to `--as`, competing for the same board, and
an unlaned unassigned task was always claimable by any of them. Lanes,
assignees and `driver_only` remain opt-in narrowing on top of that default.

## Consequences

Widening the status `CHECK` meant rebuilding the `tasks` table, and a rebuild is
the easiest place in a schema to change something nobody asked to change. The
first version of that migration gave `parent_id` an `ON DELETE SET NULL`, which
would have turned "removing a parent refuses and names its children" into
"silently orphans them". The shipped rebuild reproduces the original exactly
apart from the widened `CHECK` — same `DEFAULT 3` on priority, same four indexes,
same bare `REFERENCES tasks(id)`.

The test guarding that had to be rewritten before it was worth anything. It first
asserted the *behaviour*, and the behaviour is defended twice — the remove path
names children in code before the foreign key is consulted — so injecting the
delete rule left it green. It now reads the DDL out of `sqlite_master`, where the
clause is actually written.

Relaxing the nesting rule is safe against real data: measured 2026-08-21, all
420 parent links across the twelve live boards were already among the allowed
shapes, and the change only widens what is accepted, so a board cannot become
invalid by it.

Recording previous bodies grows the event table by the size of each replaced
plan, a few kilobytes per revision. Measured 2026-08-21, the twelve live boards
held 9.6 MB and 4,752 rows in total, so this is not the thing that will make size
matter.

Deliberately not decided: linking a plan to work that is *not* beneath it. If a
plan produces work in another epic's tree, there is no way to say so today.
`--parent` covers the common case and a second, weaker relation would compete
with it; the point at which that stops being enough is the point to revisit this,
not before.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — the work breakdown this extends
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the nesting rule amended here, and the two-answers-to-one-question rule
- [ADR-012](ADR-012-session-handoffs-and-durable-attention.md) — the same "keep the record, never overwrite it" reasoning
