# ADR-021: Settled history leaves operational indexes

**Status:** Accepted
**Date:** 2026-08-25
**Deciders:** George Yong

## Context

Kanban is an append-preserving work ledger. Tasks, notes, checkpoints, events,
handoffs and attention records accumulate by design, but default lists and the
indexes serving active work must remain small and predictable over years.
Sitreps and rules already distinguish current rows from archived history; the
rest of the ledger did not.

ADR-012 anticipated moving settled rows to a sidecar once retention became
necessary. A sidecar would now make the board's backup, restore, addressing,
foreign-key and task-detail contracts span two databases. It would reduce the
main file, but at the cost of making cold history easier to separate from its
board or omit from recovery.

## Decision

- Each board retains cold history in the same SQLite file. Archival marks old
  settled rows and removes them from operational secondary indexes using SQLite
  partial indexes (`WHERE archived=0`). The primary audit data is never deleted.
- `kanban archive --older-than-days N --as ACTOR` is the explicit retention
  operation. `--dry-run` runs the same transactional updates and rolls them back,
  returning the counts it would archive.
- A task is eligible only when it is `done` or `cancelled`, has a completion time
  at or before the cutoff, and carries no lease. Its notes, checkpoints, tags,
  events, settled handoffs, resolved attention and linked sitreps become cold in
  the same transaction. Old settled taskless records are swept by their own time.
- Default task, handoff, attention and event lists exclude archived rows.
  `--all` is the deliberate cold-history read. Exact task detail remains
  addressable by ID.
- Reads never archive. The existing nightly backup job runs a 90-day sweep for
  every present registered board before taking the SQLite online snapshots.
- A successful non-empty sweep writes one `archive_swept` audit event. A dry run
  writes nothing.

## Consequences

- Operational indexes scale with current work rather than lifetime history.
- One board remains one self-contained, copyable and restorable SQLite file.
- Cold queries may scan more rows and are intentionally explicit with `--all`.
- Archival does not reclaim table pages; it bounds secondary indexes and hot
  query surfaces. Physical compaction can be added later if measured file size,
  rather than index/query cost, becomes the problem.
- This supersedes ADR-012's tentative sidecar recommendation while preserving
  its requirement that historical accounts are not deleted.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md)
- [ADR-003](ADR-003-private-multi-project-personal-work-system.md)
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
- [ADR-012](ADR-012-session-handoffs-and-durable-attention.md)
- [ADR-017](ADR-017-a-sitrep-is-the-low-ceremony-sibling-of-a-handoff.md)
