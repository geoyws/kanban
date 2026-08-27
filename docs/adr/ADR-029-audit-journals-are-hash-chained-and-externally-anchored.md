# ADR-029: Audit journals are hash-chained and externally anchored

**Status:** Accepted
**Date:** 2026-08-27
**Deciders:** George, Kanban maintainers

## Context

Kanban already retains task, claim, checkpoint, handoff, attention, rules, archive, and override history in SQLite. Transactions, foreign keys, leases, private file modes, `PRAGMA integrity_check`, rescue snapshots, and compiled-process tests make that history durable and operationally reviewable. They do not make it tamper-evident: SQLite can remain structurally valid after an event is edited or removed, and a complete older snapshot is also structurally valid.

“Audit-safe” therefore needs a bounded threat model. We protect against application defects, accidental corruption, partial copies, unsupported direct database edits, event deletion/reordering/editing, snapshot substitution, and unnoticed rollback past a surviving external anchor. We do not claim protection from an administrator who controls both the HAX data files and every independently retained anchor. Confidentiality and host compromise remain separate controls.

## Decision

### Canonical chain

Every board `events` row and registry audit row carries `prev_hash` and `event_hash`. The hash is SHA-256 over a versioned, length-delimited canonical encoding of:

- journal domain and chain version;
- sequence number;
- previous hash;
- subject identifier, event kind, and actor;
- the exact JSON payload bytes;
- creation timestamp.

The operational `archived` projection is deliberately excluded: archival moves an immutable event out of hot indexes without changing what happened. The separate `archive_swept` event records that projection change.

Registry registration, attachment, detachment and repointing join the registry
journal. Read-driven `last_used_at` recency touches are an operational cache,
not work-state mutations, and are deliberately outside the audit contract.

The first protected row uses the all-zero genesis hash. Verification walks the complete journal in sequence order, including archived rows, and fails on a sequence gap, a previous-hash mismatch, an invalid digest, or a missing/blank actor in a post-migration row.

### Honest legacy boundary

Schema migration deterministically hashes existing history and records the protected boundary and migration time in metadata. This proves that current history has not changed *since the boundary*; it does not pretend historical rows were protected before migration. Legacy rows may retain a missing actor. Every new supported mutation must use a non-empty supplied actor or a named `system@…` actor.

### Anchors and snapshots

`kb audit verify` verifies registry and board chains. `kb doctor` includes the same verification and exits non-zero on failure. Backup writes a deterministic manifest containing every database path, byte digest, schema version, journal length, and journal head. Restore verifies the manifest and every chain before replacing live state.

A hash chain alone cannot distinguish an intact old database from the current one. A retained manifest or published head is therefore an external anchor. Rollback before that anchor is detectable; history after the newest surviving anchor is outside that guarantee. Restore remains an explicit operator action and preserves the pre-restore rescue snapshot and both old and restored heads in its receipt.

### Failure behavior

Verification is read-only and fail-closed. It never repairs or rehashes current-version history. Migration is the only operation allowed to initialize hashes for legacy rows. The verifier names the journal and first bad sequence without printing secrets or lease tokens.

## Consequences

- Ordinary corruption and unsupported edits become visible even when SQLite's structural checks still say `ok`.
- Backups become self-describing, substitution-resistant recovery artifacts.
- Actor requirements become stricter and some legacy invocations without `--as` must be updated.
- The chain adds a small constant write and verification cost, acceptable for a low-volume work ledger.
- Hash chaining is not a signature. A privileged administrator who can replace databases and all anchors remains outside the guarantee; independently retaining manifests is required for adversarial rollback evidence.
- Legacy history receives an explicit trust boundary rather than retroactive assurance.

## Evidence required

Compiled-process tests must copy a real generated ledger, mutate event content, delete and reorder rows, substitute a database in a manifested snapshot, and show that the released binary exits non-zero. Unit tests may cover canonical encoding and migration but are not E2E evidence.

## References

- [ADR-006: Rust runtime and compiled binary E2E](ADR-006-rust-runtime-and-compiled-binary-e2e.md)
- [ADR-021: Settled history leaves operational indexes](ADR-021-settled-history-leaves-operational-indexes.md)
- Epic `e-8e5f6b21`: Kanban audit-safe ledger and forensic verification
