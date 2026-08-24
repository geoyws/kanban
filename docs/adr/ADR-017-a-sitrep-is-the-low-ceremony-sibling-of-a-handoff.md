# ADR-017: A sitrep is the low-ceremony sibling of a handoff

**Status:** Accepted
**Date:** 2026-08-24
**Deciders:** George

## Context

Everything the ledger could record about *where things stand* was gated behind
something.

- A **note** needs a task (`task_notes.task_id` is `NOT NULL`).
- A **checkpoint** needs a task **and** a live lease, and refuses if the lease
  belongs to another agent.
- A **handoff** is deliberate by design: it says *I am leaving, here is
  everything you need*, releases the lease and names a successor.

So an agent working across several tasks, between them, or exploring before it
had claimed anything, had nowhere to write down what it knew. That information
went into a reply — which scrolls away — or waited for a handoff nobody had time
to write. The result is the failure
[ADR-004](ADR-004-token-pressure-handoffs-through-kanban.md) and
[ADR-012](ADR-012-session-handoffs-and-durable-attention.md) both describe from
other angles: a driver dies and everything it knew dies with it, because the
only durable places to put it were too expensive to use twenty times a day.

## Decision

**`kanban sitrep post TEXT --as AGENT --lane LANE` — keyed to a lane, needing no
task and no lease.**

Lane-keyed is the load-bearing part. A session handoff is already findable by
lane rather than by directory (`to_agent`), because worktrees get recreated and
drivers renumbered. A sitrep is the same channel at a fraction of the
cost, which is what makes it the thing an agent actually reaches for.

`--task` is optional and refuses an id that does not exist: a sitrep pointing at
nothing reads as context and carries none.

**Provenance is captured, not requested** — worktree, branch, HEAD, root HEAD,
dirty count — the same as a claim's, and for the same reason. "Tests green" that
does not say which checkout is a claim nobody can check. Run outside a
repository and it records absent rather than inventing one.

### Archiving retires, it does not delete

Posting an update archives everything past the newest ten in that lane.
Archived rows are hidden from the default read and returned by `--all`.

Three properties, each chosen against an alternative:

- **On write, not on a timer.** Nothing has to be scheduled, and the current
  view is bounded at exactly the moment it would have stopped being current. A
  sweep would need somewhere to live and something to run it.
- **Per lane.** The question is "where does *this* lane stand", so another
  driver's chatter must not push a lane's own history out of view.
- **Retire, never destroy.** A hard retention cap was written and then removed
  before it shipped. It would have been the first thing in this ledger that
  destroys a record: attention items are resolved rather than deleted, handoffs
  outlive the task they were about, and the event trail is append-only.
  Archiving bounds the **view**, which is what "old entries get archived" asks
  for. Bounding the **table** is a deliberate operator-run prune over a whole
  board, not a silent side effect of somebody posting an update. The growth this
  leaves is the growth `events` and `task_notes` already have, so singling this
  table out would have been inconsistent as well as destructive — and measured
  2026-08-24, all thirteen boards together hold 9.6 MB.

### The name is `sitrep`, not `status`

The surface first shipped as `status` in V9. That was wrong: a task's `status`
is already a workflow state from a closed set, and `--status` is a flag on
several commands. Two concepts with the same name made "post a status"
genuinely ambiguous, even though command and flag occupied different positions.

`sitrep` means situation report and collides with nothing in the repository or
the surrounding agent instructions. Its alias is `sr`. Alternatives were
rejected because they already name nearby concepts: `checkpoint` is the
lease-bound task record, `brief` is used for session handoffs, `pulse` is too
close to heartbeat, `progress` is a note kind, `log` is too close to events, and
`update` is already `task update`.

V10 renames `status_updates` to `sitreps`, rewrites the one-day-old probe ids
from `u-` to `sr-`, and rewrites `status_posted`/`statusID` in the event trail.
Rewriting ids is acceptable only because every existing row was a probe; a
mature table with real history would keep its ids and live with the seam. The
old `status` command and `st` alias are not deprecated aliases: they fail as
unknown commands under ADR-008, so an operator can never believe a sitrep was
posted when it was not.

### Where it surfaces

- **`context`** carries the sitreps mentioning a task, archived ones included: a
  resuming agent reads the packet and nothing else, so a sitrep only
  `sitrep list` could see is one its intended reader never gets.
- **The web view** gains a **Lanes** page — the counterpart to Needs you. That
  page is what waits on the operator; this is what the agents are doing.

## Consequences

The handoff does not change and is not deprecated. It remains the thing written
when a lane is actually being left: it releases the lease, names a successor and
returns the task to the queue in one transaction. What changes is that a handoff
— or a successor arriving without one — now has something to stand on.

`--all` is a new global-ish boolean, and the read-only guard exercises
`sitrep list` like every other read.

Nothing deletes a lane, either — which surfaced immediately, from probing this
on the live board and leaving eleven throwaway rows in a `probe-lane` that
cannot be removed. The Lanes page therefore orders lanes by most recent
activity, so a lane whose driver is long gone sinks out of the way without
anything destroying what it said. That is the same answer archiving gives
within a lane, applied between them.

Not offered: editing or deleting an individual sitrep. It is a record of what
was believed at a moment, and a record that can be revised after the fact is
worth less than one that cannot. Post another one; the newest is the answer.

Deliberately not decided: whether ten is the right number of current updates per
lane. It is a guess that reads well, it is one constant, and the honest way to
find out is to use it.

## References

- [ADR-004](ADR-004-token-pressure-handoffs-through-kanban.md) — the death-mid-task problem this widens the answer to
- [ADR-012](ADR-012-session-handoffs-and-durable-attention.md) — lane-keyed session handoffs, and "resolved, never deleted"
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) — the web view this adds a page to
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — why a laneless or bodyless update is refused rather than defaulted
