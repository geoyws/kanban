# ADR-024: Rules target task tags after board scope

**Status:** Accepted
**Date:** 2026-08-26
**Deciders:** George

## Context

Board scope and subsystem scope answer different questions. A board says which
project owns work; a task tag says which part of that project the work touches.
One subsystem such as `aix` can cross several product boards, while one board
such as `px` contains many unrelated subsystems. Repeating one rule per board
drifts, while injecting it into every task on those boards wastes context and
can make an inapplicable instruction look mandatory.

Tags are intentionally registered per board ([ADR-015](ADR-015-tags-are-a-per-board-master-file.md)).
Global rules already have explicit `ALL`, `ONLY:<board>`, and `EXCEPT:<board>`
targets ([ADR-020](ADR-020-global-rules-use-explicit-board-tags.md)). The new
scope must compose with both decisions without turning tags into lanes or
creating a second global tag registry.

## Decision

A project or global rule may carry zero or more task-tag selectors, exposed as
repeatable `--tag NAME` and serialized as `taskTags`. Zero preserves today's
meaning: the rule applies to every task inside its board scope. Several task
tags are an OR set: a rule matches when the addressed task carries at least one
of them.

Scope is evaluated in order:

1. resolve the board and apply the global rule's `boardTags`;
2. when `taskTags` is non-empty, require an addressed task and an intersection
   with that task's registered tags;
3. inject only the rules which pass both filters into claim and context.

Board and task-tag selectors therefore intersect; neither widens the other.
Session-level operations with no addressed task omit task-tag-scoped rules.
Rule list/show surfaces still display them so omission is observable.

Project rules validate selectors against that board's tag master. A global
rule accepts a tag only when the same canonical name is registered on at least
one active board. A later board that registers that name becomes eligible
without copying the rule. This does not make tag descriptions global: the
lowercase canonical name is the cross-board matching key, while each board
continues to own its vocabulary and description.

Updates preserve history. Rule events record old and new `taskTags`, and an
empty selector is explicit rather than inferred. Existing rules migrate with
an empty list and remain behaviorally unchanged.

## Consequences

Rules can follow a subsystem across boards without duplication and can avoid
unrelated work within a mixed board. The existing board selectors retain their
meaning, including `ALL` as a board tag; task tags cannot impersonate or replace
them.

The registry must inspect active board tag masters when validating a global
selector, and task-aware rule projection must load tags before building claim
or context receipts. Those reads are bounded by the addressed task and the
small active rule set.

A canonical tag spelling now carries cross-board semantic weight. That is
intentional but does not guarantee identical descriptions; operators should
reuse a name only for the same subsystem concept.

## References

- [ADR-015](ADR-015-tags-are-a-per-board-master-file.md)
- [ADR-018](ADR-018-project-rules-frame-work-without-replacing-private-memory.md)
- [ADR-019](ADR-019-global-rules-frame-every-project-on-claim-and-resume.md)
- [ADR-020](ADR-020-global-rules-use-explicit-board-tags.md)
