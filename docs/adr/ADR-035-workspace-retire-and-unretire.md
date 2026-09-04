# ADR-035: Workspace retire and unretire

**Status:** Proposed
**Date:** 2026-09-03
**Deciders:** Team

## Context

Boards need an explicit retired state that preserves the board identity, keeps
the archive auditable, and refuses name/root resolution against retired
authority. Default operator views should stay focused on active work, while
opt-in inspection can still surface retired boards.

## Decision

Add `kanban workspace retire NAME --as ACTOR --note TEXT` and
`kanban workspace unretire NAME --as ACTOR`.

Retirement marks the board archived in the registry, records the actor, note,
and timestamp, preserves the board's historical roots, and emits an audited
registry event. Retired names and roots fail closed in normal board resolution.

Unretire restores exactly one retired board, rejects duplicate or conflicting
name/root authority, and does so inside a single transaction so a conflict
leaves the registry unchanged.

Default `workspace list`, `dashboard`, `doctor`, `search --all-boards`, and
`search-rebuild --all-boards` stay active-only. The `--all` opt-in exposes
retired inspection where already supported by the command.

## Consequences

Retired boards remain discoverable for audit and recovery, but they do not
silently participate in everyday lookup or search.

The registry schema needs to carry retired-board metadata for active records
and history rows, and the command surface now has to keep active-only and
all-board code paths in sync.

## References

- `rust/registry.rs`
- `rust/lib.rs`
- `tests/e2e.rs`
