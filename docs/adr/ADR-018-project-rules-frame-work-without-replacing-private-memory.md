# ADR-018: Project rules frame work without replacing private memory

**Status:** Accepted
**Date:** 2026-08-24
**Deciders:** George

## Context

The work ledger can resume a task from its claim, context, checkpoints and
handoffs, but short project-wide operating constraints still live outside that
resume path. An agent can therefore claim valid work without seeing the rule
that determines how the project must be handled.

Copying all private project memory into every claim would solve the visibility
problem by creating a worse one. Dotfiles remain the versioned, encrypted and
cross-machine home for long-form context, credentials and secrets. Kanban boards
are operator-private and locally served, but their SQLite files are plaintext
working state and are not a secret store.

## Decision

Each project board has an ordered, audited, retire-only rules document.
`kb rule add|list|show|update|retire` manages it; `r` is the exact-match command
alias and `ls`, `new`, `cat` and `up` are its non-destructive subcommand aliases.
There is deliberately no remove command or `rm` alias. Updates preserve the
previous body in the event trail, and retirement removes a rule from the active
document without deleting its history.

A rule's first line is its headline. Every `kb context` packet and every newly
granted claim carries the complete, untruncated table of contents for active
rules: id, headline, byte size and whether more detail exists. Long bodies stay
lazy behind `kb r cat ID`. The claim receipt remains flat for wire compatibility;
an ordinary stored claim does not grow a `rules` field, because an empty field
there could falsely imply that the board was checked.

The read-only board page displays active rules in expandable details. Both the
headline and full body are HTML-escaped. Retired rules are absent from context,
new claims and the served page, but remain available through `kb r ls --all
--full` and the append-only event trail.

Rules are for short, non-secret operating constraints that should frame every
piece of work in one project. Long explanations, durable findings, credentials,
secrets and material that must sync across machines remain in the versioned and,
where needed, git-crypt'd dotfiles. A board rule may point there, but secret
values must never be copied into the plaintext board database.

## Consequences

- A newly claimed or resumed task is framed before work begins, without an
  agent having to know which external file to search.
- Context cost stays bounded by carrying an always-complete table of contents
  and fetching full long rules only on demand.
- Rules gain an auditable lifecycle and a human-readable served surface.
- Operators must still maintain dotfiles for long-form, encrypted and
  cross-machine knowledge; project rules complement that store rather than
  replacing it.
- The active rules document is board-local. Moving it between machines requires
  the board's normal private synchronization, not Git history in a product repo.

## Verification

Compiled-binary E2E coverage crosses real process and SQLite boundaries for the
ordered lifecycle, exact body-file fidelity, revision trail, active-only
filtering, context framing, flat claim receipts, lazy long-body summaries and
escaped served HTML. Unit tests remain a separate layer.

## References

- [ADR-005](ADR-005-kanban-owns-work-state-atmux-consumes-it.md)
- [ADR-007](ADR-007-global-project-addressing.md)
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md)

