# ADR-007: Address projects globally by name, not only by working directory

**Status:** Accepted
**Date:** 2026-08-18
**Deciders:** George

## Context

Storage was already global. [ADR-003](ADR-003-private-multi-project-personal-work-system.md)
puts every board under the operator's private data directory and maps workspace
roots to boards in one registry.

Addressing was not. Every board command resolved its board by walking up from
the process working directory, so a command run anywhere else failed with
`no Kanban workspace contains the current directory`. The only escapes were
`--db PATH` and `KANBAN_DB`, both of which require knowing a board's UUID file
name — an implementation detail the operator should never have to hold.

That made the CLI unusable from exactly the places agents run: a cage rooted in
an unrelated repository, a shell in `$HOME`, a cron line, a scratch directory.
The workaround is to `cd` first, which couples every caller to a filesystem
layout it has no other reason to know. A `--workspace PATH` flag already existed
and was honoured by `init` and `workspace attach` only, so the same flag meant
"the project" for two commands and nothing at all for the rest.

## Decision

Board selection is an explicit chain, most explicit first:

1. `--db PATH` or `KANBAN_DB` — a board file directly.
2. `--project NAME` or `KANBAN_PROJECT` — a registered project by registry name,
   from any directory.
3. `--workspace PATH` — the project containing `PATH`, now honoured by every
   board command rather than two.
4. The working directory — the project containing it.

Rule 4 is unchanged, so every existing caller keeps working; the change is
purely additive. Rule 2 is the global route: an agent exports `KANBAN_PROJECT`
once per cage, or passes `--project`, and never needs to know where a board
lives or where it is standing.

Registry names are not unique — two projects may share one. An ambiguous
`--project` is an error naming the candidate roots, never a pick. Silently
choosing would write work state to the wrong board, which no later command can
detect or undo. `--workspace` disambiguates what a name cannot.

A failure to resolve names the global route and lists the known projects. An
error that only says "run `kanban init`" teaches the operator to `cd`, which is
the behaviour this ADR removes.

Name-based resolution updates `last_used_at`, which path resolution already did
as a side effect of its walk. Without that, a project addressed only by name
would sink to the bottom of the dashboard's recency ordering while in active use.

## Consequences

The CLI is usable from anywhere on the host, and callers stop encoding
filesystem layout. `--project` and `--workspace` are equally available to reads
and writes, so a mistyped project name is a refusal rather than a wrong-board
write.

Two projects sharing a name are now a diagnosable condition rather than an
invisible one: any `--project` use surfaces it immediately.

Deliberately not decided: a persisted "current project" (`kanban use NAME`). A
default stored in the data directory is global mutable state shared by every
process on the host, so one agent switching it would silently retarget every
other agent's bare commands. `KANBAN_PROJECT` gives the same ergonomics scoped
to the shell or cage that set it, which is where the intent actually lives.

## References

- [ADR-003](ADR-003-private-multi-project-personal-work-system.md)
- [ADR-005](ADR-005-kanban-owns-work-state-atmux-consumes-it.md)
