# ADR-004: Perform agent handoffs through Kanban

**Status:** Accepted
**Date:** 2026-08-14
**Deciders:** George

## Context

Agents have bounded context and token budgets. When an active agent is nearing
that boundary, another agent must be able to continue without relying on chat
history, terminal scrollback, or a repository-local `HANDOFF.md`. A checkpoint
records progress, but a handoff also needs an explicit transfer of ownership
and an auditable acknowledgement by the replacement agent.

## Decision

An agent handoff is a first-class Kanban transition.

The outgoing agent creates a handoff while it still owns the task lease. In one
SQLite transaction Kanban:

1. validates the active lease and outgoing identity;
2. records the reason, normally `token_pressure`;
3. appends a structured continuity checkpoint containing summary, intent,
   next action, blockers, validations, and repository state;
4. creates a pending handoff record; and
5. releases the old lease and makes the task claimable.

The replacement agent accepts the pending handoff. In one transaction Kanban
validates eligibility, creates a fresh lease, marks the handoff accepted, and
moves the task back to `in_progress`. The replacement starts by loading the
task context, which includes the newest handoff and checkpoint.

If another transition has since made the task `blocked`, `done`, or `cancelled`,
acceptance is only an acknowledgement that the brief was absorbed. It marks the
handoff accepted but preserves the task status and creates no lease. Otherwise
old correspondence would remain permanently pending precisely when there is no
claimable work left to protect.

Handoffs may name a target agent, but an unnamed target is valid when the next
compatible agent should pull the work. Lease tokens are authorization material
and are never stored in handoff prose or model prompts.

**Compatibility amendment (2026-09-20).** A driver lane follows atmux's
`^driver(?:-[1-9][0-9]*)?$` grammar. New handoffs may not address that lane by
its bare name: `--to driver-2` is refused with
`handoff target driver-2 is a bare lane name; pass the full typed actor as --to @:team/project/driver-2`.
The writer must store the full `@:team/project/lane` actor instead. This is
specific to driver-lane names; ordinary untyped identities remain supported.

A typed `@:team/project/<lane>` actor may nevertheless accept a pending legacy
row whose `to_agent` is exactly the same bare final lane segment. Acceptance
records the full actor in `accepted_by` and does not rewrite `to_agent`. This is
a one-way read compatibility rule: a wrong typed lane is refused, a stored
typed target still requires byte-for-byte actor equality, and an untyped actor
gains no alias. The create rejection and legacy acceptance use one shared
parser, so the lane grammar cannot drift between the two sides of the cutover.

Repository-local handoff files are not part of the continuity contract.

## Consequences

Token exhaustion becomes a recoverable, auditable transition rather than an
informal message. No two agents own the same task lease, and incoming agents
receive enough durable state to resume cold.

Outgoing agents must hand off before their context is exhausted. If an agent
dies without doing so, normal lease expiry still permits recovery, but the last
checkpoint may be less current and no explicit handoff acknowledgement exists.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md)
- [ADR-003](ADR-003-private-multi-project-personal-work-system.md)
- [Product requirements](../PRD.md)
