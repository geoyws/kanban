# ADR-012: Handoffs that are not about a task, and attention that outlives the session

**Status:** Accepted
**Date:** 2026-08-21
**Deciders:** George

## Context

Two things agents produce had nowhere durable to go.

**A handoff was always about one task.** `handoffs.task_id` and
`checkpoint_seq` were both `NOT NULL`, and `handoff create` required a live
lease, so the only handoff that could exist was the record of a lease changing
hands. That is the right shape for "I ran out of context mid-task" and the wrong
shape for the more common case: a lane finishing a session with work spread over
several tasks, or none — the state of a worktree, what was learned, what the
successor should do first. That brief was being written to files outside the
ledger, keyed by directory, which breaks the moment a lane moves. A successor
knows *which lane it is*; it does not reliably know which directory the previous
session was standing in.

**Handoff history died with its subject.** `task_id` was `ON DELETE CASCADE`, so
removing a task deleted every handoff ever taken over it. A handoff is an
account of a handover that happened, and deleting the task does not un-happen
it.

**Things needing the operator had no home at all.** Agents surface blockers,
decisions and approvals in reports, commit messages and chat replies — every one
of them a channel that scrolls away. An item raised at 03:00 and not acted on
leaves no trace it was ever raised, so the same question is asked again three
sessions later or, worse, quietly answered by an agent that should not have
decided it.

## Decision

### A handoff may be about the session

`task_id` and `checkpoint_seq` become nullable. Without a task id,
`handoff create` takes no lease, writes no checkpoint, and disturbs no task
state; it records the brief and who it is for.

The task id and the lease travel together and are resolved as a pair: a lease
exists only over a task, and a task cannot be handed over without one. Each half
alone is refused by name. This is enforced in code rather than as a CHECK,
because it is a rule about how a handoff is *made* and must not hold forever —
see below.

`handoff list --to AGENT` is what makes a session handoff findable. A task
handoff is reachable through its task; a session handoff is about no task, so
without it the only route would be reading the whole list. The successor knows
its own identity, and that is the key it should look itself up by — `--project`
selects the board, `--to` selects the lane, and no directory is involved.

Accepting a session handoff is an acknowledgement: it records who picked the
thread up and stops it being offered again. There is no task, so no lease is
minted, and `claim` comes back null.

### The record outlives what it describes

`task_id` and `checkpoint_seq` are `ON DELETE SET NULL`, not `CASCADE`. Removing
a task drops the links and keeps the account. That is also why the creation-time
pairing is not a CHECK constraint: a handoff whose task was removed is
legitimately half-linked, and a CHECK would either forbid the removal or force
the history to be destroyed to satisfy it.

The rebuild disables foreign keys for the migration only. `PRAGMA foreign_keys`
is a no-op inside a transaction, which is where migrations run, so it is set
around the whole ladder in `open_board` and restored before the connection does
any work. This is SQLite's documented procedure for rebuilding a table, and it
is required here for a second reason: a v3 board written by the retired
TypeScript implementation may lack `checkpoints` entirely, and with enforcement
on, the rebuild reads the tables its references point at.

### Attention is a first-class, durable record

`kanban attention raise|list|resolve`. An item carries its kind, its text, who
raised it, when, and optionally the task it concerns.

`kind` is a closed set — `blocking`, `decision`, `approval`, `review`, `risk` —
because "what sort of thing is this" is the part a reader needs first and the
part free text would not answer. There is deliberately no `info`: something that
needs nobody is a note, and `task note` already holds those. Everything here is
something only the operator can retire.

Items are **resolved, never deleted**, and resolving one twice is refused, since
that would overwrite who settled it and when — the part worth keeping. Listing
puts open items first and *oldest* first within each state, which is the
opposite of every other listing here and deliberate: an unanswered question does
not get less urgent by being ignored, and newest-first buries the item that has
waited longest. `dashboard` reports the open count so an operator sees it
without asking, and both raising and resolving write to the event trail.

## Consequences

A session's brief lives on the board, addressed to a lane, and survives the
directory it was written in. `/session cont` can ask "what is waiting for
driver-2 on this project" instead of deriving a path.

Both new surfaces reach MCP for free: tools are generated from `COMMANDS`
([ADR-010](ADR-010-adapters-generated-from-the-command-surface.md)), so
`attention_raise`, `attention_list`, `attention_resolve` and the widened
`handoff_create` appear with correct schemas and `readOnly` classification, and
the test that proves those labels runs the new read-only operation too.

Rows accumulate, because that is what a trail does. Measured 2026-08-21 across
the twelve live boards: 9.6 MB and 4,752 rows, about 2.1 KB per row. Reaching a
million rows would take roughly two hundred times the current total, and SQLite
is unbothered well past that. No archiving is built, and none is needed yet;
when it is, it belongs as a per-board command moving settled rows to a sidecar,
not as a different database. Moving to a server would cost the property the rest
of this design rests on — a board is a file that can be copied, backed up,
addressed by path and handed to another machine with nothing running.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — the durable resume contract
- [ADR-004](ADR-004-token-pressure-handoffs-through-kanban.md) — task handoffs
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the pairing and refusal rules
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — why the new commands reach MCP unaided
