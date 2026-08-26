# ADR-027: Rules are one tag-scoped `/kb` document

**Status:** Accepted
**Date:** 2026-08-26
**Deciders:** George
**Supersedes:** ADR-018 project-rule storage and ADR-019 global/project composition

## Context

Kanban currently has two rule species. Board databases own `r-*` project rules,
while `registry.db` owns `g-*` global rules. Later decisions added board-selector
tags and task-subsystem tags to the global rows, and task-subsystem tags to the
project rows. Applicability is therefore already tag-shaped, but storage, CLI,
JSON, documentation, and rendering still require agents to choose or understand
a project/global scope that no longer carries useful meaning.

Boards remain the correct boundary for work ownership, task routing, leases, and
storage. They are not the correct identity for reusable operator rules. A rule
about `aix`, deployment, or test evidence may cross boards; a board may contain
many unrelated subsystems. Keeping both scope and tags makes the same rule either
too broad, duplicated, or dependent on which table an operator happened to use.

## Decision

Kanban has one registry-owned rules document. Every row is simply a `/kb` rule,
and one ordered `tags` array expresses applicability:

- `ALL` selects every registered board and is the default selector;
- `ONLY:<board>` selects an exact registered board; several form an include set;
- `EXCEPT:<board>` subtracts exact boards from `ALL`;
- lowercase registered names select task subsystems; several form an OR set;
- board selectors and subsystem selectors intersect when both are present;
- a taskless context omits rules with subsystem selectors because no match can
  be established.

The syntactic families make interpretation deterministic. `ALL`, `ONLY:`, and
`EXCEPT:` are reserved rule tags and never enter a board tag master. Lowercase
subsystem tags continue to be validated against the union of active board tag
masters. `ALL` cannot coexist with `ONLY:*`; `EXCEPT:*` requires `ALL`; duplicate,
unknown, or ambiguous selectors fail closed.

The public model exposes `tags`, not separate `boardTags`, `taskTags`, or
`scope`. Existing `g-*` identifiers remain valid historical identifiers but no
longer imply global scope; newly created rules use `r-*`.

Registry schema v9 creates canonical `rules` and `rule_events` tables and copies
the existing registry rows into them, joining their selector and subsystem arrays
without changing order. Board-local rows require an explicit, idempotent
cross-database consolidation because a SQLite migration cannot transact across
all board files. Consolidation:

1. copies each row with `ONLY:<canonical-board>` plus its task tags;
2. preserves its active/retired state and records source board and source ID;
3. reuses the source ID when free and allocates a new `r-*` ID on collision;
4. writes a canonical migration event containing the source identity;
5. retires an active source row with a board event, never deleting its body or
   revision trail; and
6. is repeat-safe through a unique `(source_board, source_rule_id)` key.

The release must consolidate every reachable live board before switching the
served binary. Missing/unreadable boards are a failed migration, not silently
skipped rules.

## Consequences

Agents and operators learn one rule concept and one CLI surface. A rule can move
from universal to one board, all-but-one board, or a subsystem crossing boards by
changing tags rather than changing storage class. Claim, context, handoff, MCP,
search, and web projections share one resolver and cannot disagree about scope.

The registry becomes the authoritative single-machine working copy for short
operational rules. Long, secret, or cross-machine material remains in encrypted
dotfiles; this ADR does not make plaintext board state suitable for credentials.

Historical board rule tables remain in old board schemas so audit history stays
readable and older backups remain restorable. They are compatibility history,
not an active rule source after consolidation.

## References

- [ADR-015](ADR-015-tags-are-a-per-board-master-file.md)
- [ADR-018](ADR-018-project-rules-frame-work-without-replacing-private-memory.md)
- [ADR-019](ADR-019-global-rules-frame-every-project-on-claim-and-resume.md)
- [ADR-020](ADR-020-global-rules-use-explicit-board-tags.md)
- [ADR-024](ADR-024-rules-target-task-tags-after-board-scope.md)
