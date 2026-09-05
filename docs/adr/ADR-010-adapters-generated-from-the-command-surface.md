# ADR-010: Adapters are generated from the command surface, not written beside it

**Status:** Accepted
**Date:** 2026-08-20
**Deciders:** George

## Context

[ADR-001](ADR-001-durable-agent-work-ledger.md) §6 settles what consumers get:
narrow operations, never arbitrary write SQL, and "future MCP and plugin
adapters expose the same operations". It does not say how an adapter learns what
those operations are.

The only available answer was to read the source or the help text and restate
the surface in the adapter. That produces a second description of something that
already has one. It agrees on the day it is written and drifts from the first
change afterwards — a flag added here, a command renamed, a single-valued flag
becoming a list — and it drifts *silently*, because nothing compares the two.

The failure is worse than a stale document. An adapter that generates a tool per
operation and gets one flag wrong hands an agent a tool that fails on every
call, or worse, one whose `--depends-on` accepts a single value where the CLI
takes a list, so the agent's second dependency is dropped with no error. This
repository already refuses that shape of defect everywhere else it appears.

There is also nothing an adapter can use to withhold mutation. ADR-001 §6 says
agents do not receive unrestricted mutation access, and the only way to honour
that was for each adapter to maintain its own list of which commands write —
a third description, drifting independently of the other two.

## Decision

**`kanban schema --json` emits the operation surface as data, projected from
`COMMANDS`.** That table is already the single description the parser validates
against; the manifest is a view of it, not a copy. Adding a command or a flag
updates the manifest by construction, and a command added without its flag list
still fails the existing drift guards.

Each operation carries its name, command and subcommand words, its positional
arity, its flags, and each flag's kind — `value`, `boolean`, or `list`. The kind
is what stops an adapter from generating a tool that silently drops the second
element of a list-valued flag.

**Each operation declares whether it is read-only, and read-only means it writes
nothing anywhere** — not the board, not the registry, not a file. `backup` and
`todo` are therefore not read-only despite changing no work state, because they
create files. The stricter reading is the useful one: a harness withholding
mutation wants to know what can touch the disk, not what can touch a table.

**The label is tested, not asserted.** An e2e records the board file's bytes,
runs every operation the manifest calls read-only, and requires the file to be
identical afterwards. It also requires every read-only operation to be
exercised, so a new one cannot be labelled without proving it. A capability flag
an adapter trusts to gate writes is a security property, and one that is merely
declared is worth nothing.

**Amended 2026-09-05 (geoyws, kanban a-3902225d): a record the caller cannot
cause is not the caller's write.** "Writes nothing anywhere" is about what the
*caller* can change. A broker that authorizes an operation appends its own
audit row for its own decision (ADR-033), about the caller, and the caller
cannot cause, shape, or suppress it; a registry recency touch is the same shape
(ADR-029). Neither makes the operation `readOnly: false`. Read literally
without this, every brokered operation — `task list` included — would be
`false`, and a flag that is `false` for everything tells a harness nothing,
which is exactly the worthlessness the paragraph above refuses.

The strict reading is unchanged for anything the caller reaches: `backup` and
`todo` still create files at the caller's request and are still not read-only.
The tested property is unchanged too — the e2e measures the board file's bytes,
and a broker's audit row is not in that file. This narrowing lives here, once,
so no downstream document redefines the term; ADR-038 cites it rather than
restating it.

## Consequences

An MCP server, an orch plugin, or an atmux adapter generates its tool list from
`schema` and inherits changes to the CLI rather than tracking them. None of them
needs a Kanban dependency beyond the binary and its manifest.

`COMMANDS` rows gained a fifth field, so adding a command now means saying
whether it writes. That is deliberate: the classification is the part an adapter
depends on, and the compiler will not let it be forgotten.

The manifest describes the surface, not the semantics. It says `task move` takes
a status positional; it does not say which statuses are legal, or that a story's
status is projected from its gate. An adapter still surfaces the CLI's errors
rather than trying to pre-empt them, which is correct: the validation lives in
one place and stays there.

Deliberately not decided: whether Kanban ships an MCP server itself, and over
what transport. The manifest is what any such server must be built from, and it
exists now; the server is a separate decision about what this binary should be.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) §6 — narrow operations, not arbitrary write SQL
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the single-description rule these guards come from
