# ADR-026: Claim candidates are a read-only scheduler view

**Status:** Accepted
**Date:** 2026-08-26
**Deciders:** George

## Context

atmux `_superbot` and other dispatchers need to compare eligible work before
choosing a worker. `task list --status todo` is not that queue: it includes
containers, unmet dependencies, work beneath a draft epic, actively leased
rows, incompatible assignees and driver-only work. Reimplementing those rules
in each orchestrator would create several schedulers that inevitably disagree.

Inspection also cannot behave like an ordinary board read. Opening a board may
migrate its schema and sweep expired leases, while registry resolution updates
recency. Those are useful normal semantics but they make a scheduler preview a
write with surprising state transitions.

## Decision

`claim --candidates --as AGENT` exposes the ordered rows that the atomic
`claim --next` scheduler may select for that identity and routing policy.
`--lane`, `--role`, `--caller-scope`, `--no-cross-lane` and
`--allow-reassign` retain their claim meanings. `--tag` narrows the result to a
registered subsystem tag, and `--limit` bounds it. Explicit `--project`,
`--workspace` and `--db` addressing remain available.

Candidate inspection and `claim --next` call one eligibility function. The
result contains ordinary task fields, including ID, tags, lane, assignee,
priority and `driverOnly`; it contains no claim or lease token.

The command uses read-only SQLite connections for both registry and board. It
does not create or migrate databases, touch registry recency, sweep expired
leases, alter task status, append events, or cache results. If either schema is
old, inspection fails and asks for an ordinary command to migrate it. Returned
rows are snapshots, not reservations: dispatch still requires an atomic claim
and must tolerate another worker winning the race.

## Consequences

- Orchestrators can make informed choices without duplicating Kanban policy.
- Preview traffic remains safe under many concurrent agents.
- Expired leases are not cleaned by preview; an ordinary command performs that
  maintenance before the row becomes visible as `todo` again.
- Compiled-process E2E must prove exclusion parity, canonical ordering,
  read-only board and registry state, explicit project addressing, absence of
  lease material, and immediate claimability of every returned row.
