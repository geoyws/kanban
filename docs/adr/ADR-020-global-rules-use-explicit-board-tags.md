# ADR-020: Global rules use explicit board tags

**Status:** Superseded in part by ADR-027
**Date:** 2026-08-24
**Deciders:** George

ADR-027 retains `ALL`, `ONLY:<board>`, and `EXCEPT:<board>` as selector tags,
but removes the global-rule species and the separate `boardTags` public field.
The scoped commands below are historical.

## Context

ADR-019 introduced one global rules document inherited by every project. Some
constraints are organization-wide, while others span several projects or every
project except one. Copying such rules into project databases reintroduces the
drift that global rules removed. Injecting every global rule everywhere also
spends context on irrelevant constraints.

## Decision

Every global rule carries explicit board tags. Their canonical representation
is returned as `boardTags`:

- `ALL` targets every board.
- `ONLY:<name>` targets one exact registered board name; several `ONLY` tags
  form an include set.
- `EXCEPT:<name>` subtracts a board from `ALL`.

The safe operator surface avoids punctuation-heavy tag syntax:

```bash
# ALL (also the default when no targeting flags are supplied)
kb r new "Universal rule." --global --as geo

# ONLY:kanban, ONLY:unum
kb r new "Shared rule." --global --board kanban --board unum --as geo

# ALL, EXCEPT:project-a
kb r new "General rule." --global --except-board project-a --as geo

# Retarget without changing the body
kb r up g-12345678 --global --board kanban --as geo
```

`--board` and `--except-board` are repeatable. Names are case-sensitive and
must identify exactly one registered board. `ALL` plus named `--board` values,
an exclusion from an `ONLY` set, duplicates, unknown names and
`--except-board ALL` are refused.

Existing global rules migrate to `ALL`. New global rules with no targeting
flags also receive `ALL`; absence is never overloaded to mean universal scope.
An update retains the current tags unless a targeting flag is supplied. Target
changes preserve the previous tags in the registry audit trail.

Claim, context and accepted-handoff injection filters global summaries using
the addressed board name, then appends that board's project rules. The web view
does the same and shows canonical board tags beside each applicable global rule.
The registry remains the only storage location; targeted rules are not copied
into project databases.

## Consequences

- One audited rule can cover one board, a named set, or all but named boards.
- Agents spend tokens only on global constraints that apply to their board.
- Historical note: this ADR originally allowed board rename or deletion to
  leave unmatched selector tags. ADR-027 now supersedes that operational
  consequence: workspace retirement refuses active `ONLY:<board>` and
  `EXCEPT:<board>` blockers. Legacy or manually introduced stale active rows
  remain inspectable and make `doctor` unhealthy until the operator updates or
  retires them.
- Direct unregistered `--db` boards inherit `ALL` rules and cannot match
  `ONLY:<name>` because they have no registered name.

## Verification

Compiled-process E2E coverage migrates a v3 registry rule to `ALL`, registers
three distinct boards, proves the exact include/exclude sets on real claims and
contexts, retargets a rule without changing its body, verifies prior tags in
the audit trail, exercises refusal cases, and checks the real HTTP server hides
non-applicable rules while rendering applicable tags.

## References

- [ADR-007](ADR-007-global-project-addressing.md)
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md)
- [ADR-019](ADR-019-global-rules-frame-every-project-on-claim-and-resume.md)
