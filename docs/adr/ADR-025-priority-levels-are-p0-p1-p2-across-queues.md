# ADR-025: Priority levels are P0, P1 and P2 across queues

**Status:** Accepted
**Date:** 2026-08-26
**Deciders:** George

## Context

Tasks already carry an integer priority from `0` through `9`, ordered lowest
first and defaulting to `3`. Attention items and handoffs do not carry priority,
even though all three are queues competing for an operator's or agent's next
action. Operators consequently have to infer urgency from kind, prose, age or
the page on which a row appears. Those signals are not interchangeable: a
blocking attention item can be important without being an interrupt, and an old
handoff can be routine.

Replacing the task field outright would discard useful ordering and break
existing CLI and JSON consumers. Leaving the scale numeric would continue to
make `1` versus `2`, or `7` versus `8`, look more meaningful than the decisions
people actually make.

## Decision

Kanban has three canonical operator-facing priority levels on every actionable
queue:

- **P0**: interrupting or blocking work that must be handled before normal work;
- **P1**: work committed to the current operating cycle;
- **P2**: routine backlog work, and the default for newly-created rows.

This applies to tasks (including stories and epics), attention items and task or
session handoffs. Sitrep entries, notes, checkpoints and events are history or
context rather than queues and do not gain priority.

The existing task integer remains the stored compatibility and ordering key.
The levels project over it as follows:

| Stored priority | Displayed level | Symbolic write anchor |
| --- | --- | --- |
| `0`–`2` | P0 | `0` |
| `3`–`5` | P1 | `3` |
| `6`–`9` | P2 | `6` |

Existing rows are not rewritten. Their exact numeric value remains the
within-level ordering key. New rows default to `6` (P2). A symbolic write of
`P0`, `P1` or `P2` stores its anchor. Task commands continue to accept `0`–`9`
for compatibility and controlled within-level ordering; attention and handoff
commands accept the same input contract so the queues do not develop subtly
different meanings. Symbols are case-insensitive on input and are rendered
uppercase.

JSON retains the numeric `priority` field and adds `priorityLevel` with `P0`,
`P1` or `P2`. Text, tables and web pages lead with the symbolic level. A legacy
out-of-band value remains readable and is reported with a null level; ordinary
writes may not create another one. This follows the preservation rule in
[ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md).

Every actionable queue sorts by stored priority ascending, then creation time
ascending, then stable row identity. This preserves existing task behavior,
retains intentional within-level ordering and makes ties deterministic.
Priority is independent of workflow status, attention kind, task lane and
handoff recipient: none of those fields silently raises or lowers it.

## Consequences

Agents and operators can scan Plans, Needs you, CLI lists and handoff queues
with one vocabulary, and urgent work can reliably lead each surface. Existing
task consumers keep their numeric field while newer consumers can use the
explicit level instead of reimplementing the mapping.

Changing the default from legacy `3` (now displayed P1) to `6` means newly
created work is routine unless its creator deliberately commits or escalates
it. Existing defaulted tasks remain P1 so migrations do not silently reprioritize
history.

The schema and indexes for attention items and handoffs must gain a priority
column, and every writer, importer, projection and queue surface must preserve
or expose it. Tests must prove ordering and backward-compatible JSON across the
compiled process boundary; an in-process store test is not E2E evidence.

## References

- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
- [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md)
- [ADR-017](ADR-017-a-sitrep-is-the-low-ceremony-sibling-of-a-handoff.md)
