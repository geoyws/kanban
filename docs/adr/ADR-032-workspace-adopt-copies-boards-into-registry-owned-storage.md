# ADR-032: Workspace adopt copies boards into registry-owned storage

**Status:** Accepted
**Date:** 2026-09-03
**Deciders:** George

## Context

Kanban already supports registering a newly created board with `init`, and it
supports attaching or detaching workspace roots from a registered board. That
left one important gap: an existing board file that already contains valid work
history could not be brought under registry ownership without copying it by
hand or reinitializing it in place.

That gap is unsafe. A source board may be live, may already have a WAL, and may
be older than the current schema. Validation, hashing, and backup performed as
separate reads can observe different commits. Byte-copying a live SQLite/WAL
set, or publishing a registry row before the destination is validated, can also
leave the registry or source board in a half-adopted state.

## Decision

`workspace adopt` becomes the canonical way to bring an existing board file
into registry-owned storage.

The command:

- requires an explicit `--as ACTOR` for audited provenance;
- requires the source argument to name the exact regular file, rejecting a
  symlink at the supplied `--from-board` path and `..` parent traversal instead
  of silently changing source identity;
- opens the source database and existing WAL with `O_NOFOLLOW`, verifies that
  each opened handle is a regular file with the same device and inode as its
  preceding `lstat`, and never reopens the source database path;
- captures the pinned main-database bytes and a stable complete WAL prefix into
  a private staging directory. A changed main-file identity/length/mtime or a
  reset WAL identity/header/length refuses the attempt; a concurrently appended
  commit remains outside the already captured valid prefix;
- opens SQLite only on that private capture. This preserves committed WAL state
  without `immutable=1`, while preventing SQLite from creating or modifying a
  source `-shm` or any other source-side sidecar;
- validates the private snapshot's SQLite integrity, `foreign_key_check`, audit
  chain, supported schema, and exact `board_meta.name`, then online-backs up
  that same read transaction into a normalized snapshot connection;
- completes all source preflight before creating the live data root, lock,
  registry database, or `boards/` directory;
- online-backs up the prepared connection into one fresh staged destination,
  migrates and validates that same connection, checkpoints it, and hashes the
  pinned database handle;
- records one durable adoption marker under the registry root before the
  destination board is published, then removes it only after the registry
  commit succeeds; startup reconciliation replays or cleans any incomplete
  attempt before the next open;
- opens every registry path component and `boards/` itself as a no-follow
  directory, pins the actual `boards/` handle, and publishes the exact hashed
  inode with `renameat` into a fresh UUID name directly below that handle;
- reverifies the complete registry path chain against the pinned `boards/`
  identity before the registry transaction commits, so a symlink or directory
  swap cannot redirect destination creation;
- records `sourceSha256` and `sourceBytes` from the exact migrated database
  inode that is registered, not from a later path reopen;
- migrates and validates the destination only, never the source;
- registers the board and, when supplied, one exact workspace root only after
  destination SQLite integrity, foreign-key integrity, audit, schema, and name
  checks succeed;
- writes one immutable `board_adopted` registry event containing the source
  board path, registered-snapshot SHA-256 and byte count, adopted board path, board
  name, and root path or null root;
- supports `--rootless` for adopted boards that should have no registered root.

Duplicate active board names, contradictory `--workspace`/`--rootless`
arguments, source corruption, audit-chain failure, and newer source schemas are
all refused.

## Consequences

The registry owns a first-class adoption path for existing boards. A commit
that lands after the pinned source capture does not slip into the validated
snapshot. Read-only source capture does not touch source metadata or sidecars,
and the recorded hash and byte count are derived from the same destination
inode that becomes the registered board.

Failure handling is deliberately fail-closed:

- the source board is never rewritten as part of adoption;
- the registry does not get a partially inserted board row;
- a destination created by the attempt is removed again if later validation or
  registry insertion fails;
- a crash after the marker is written but before the commit is reconciled on
  the next startup before any new registry work proceeds;
- cleanup removes the destination database plus every SQLite sidecar that can
  belong to the failed attempt: `-wal`, `-shm`, and `-journal`;
- only the destination created by the current attempt is cleaned up, so an
  unrelated board file is never deleted;
- cleanup errors are returned with the retained artifact paths; cleanup never
  silently converts a partial or retained staging artifact into success.

Source identity is deliberately path-strict. The final `--from-board` component
must itself be a regular non-symlink file, `..` is refused, and the opened
handle's device and inode must match the checked path. Registry destination
identity is stricter still: caller-controlled symlink and non-directory
components are refused during the no-follow walk, and publication is relative
to the pinned directory handle rather than reconstructed from a string path.

The main cost is stricter behavior. A source board with a mismatched name, a
newer schema, a broken audit chain, or a duplicate active registry name is
refused instead of being implicitly repaired.

## References

- `rust/lib.rs`
- `rust/registry.rs`
- `rust/model.rs`
- `rust/db.rs`
- `rust/audit.rs`
- `tests/e2e.rs`
- `README.md`
- `docs/testing/compiled-rust-e2e-matrix.md`
