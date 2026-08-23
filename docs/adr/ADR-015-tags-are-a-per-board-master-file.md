# ADR-015: Tags are a per-board master file, not free text

**Status:** Accepted
**Date:** 2026-08-23
**Deciders:** George

## Context

The board could say what a row *is* — epic, story, task — what state it is in,
and which lane routes it. It could not say what part of the **system** it is
about. On Unum that is the question actually being asked: is this infra, is it
the queuer, is it askie. Answering it meant reading titles.

Two existing columns looked like they might already carry it, and neither does.

**`lane` is routing, not subject.** `claim --next` selects on it, and drivers are
assigned lanes; a lane is *who picks this up*. Measured 2026-08-23 across the
thirteen live boards — 3,334 tasks — the values are `be` 611, `misc` 526, `fe`
212, `test` 202, `ops` 191, `review` 111, with 1,312 unlaned. That is a
discipline axis. Writing `infra` or `askie` into it would make `--next` route on
subsystem, quietly changing which driver receives what.

**`metadata` is free JSON, and free is the problem.** It already holds import
artifacts (`atmuxExtra`, `importedFrom` on 1,211 rows). A subject recorded there
has no spelling anyone agreed on, no way to enumerate what exists, and no way to
tell a typo from a new category.

That last point is the whole decision. A label anyone can invent at write time is
how one subsystem ends up spelled four ways — `infra`, `Infra`,
`infrastructure`, `infra-` — and a board that answers "show me infra" with three
of the four is **worse than a board with no tags at all**, because the answer
looks complete. This ledger exists to refuse fields that say something and hold
something else.

## Decision

**A tag is a row in a per-board `tags` table — a master file — and only a
registered tag can be attached.** `tag add` registers, with an optional
description and an actor; `tag list` enumerates the vocabulary with a use count
per entry; `tag remove` retires one.

**Attaching an unregistered tag is refused**, naming the nearest registered tag
and the command that would make this one real — the same shape as a mistyped
flag ([ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)),
because it is the same mistake. Registering is one command, so the cost of the
rule is a keystroke and the benefit is that the vocabulary is always
enumerable.

**Names are lowercase letters, digits and inner hyphens.** `Infra` is refused
rather than folded to `infra`: folding decides on the caller's behalf which
spelling was meant, and the point of a registry is that the question never
arises. Leading and trailing hyphens are refused for the same reason — they read
as the same tag as their trimmed form.

**Every row type carries tags** — draft, epic, story, task. The axis is "which
subsystem", and a plan belongs to one as much as the task it produces does. This
is what makes tags compose with [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md):
a drafted plan can be filed under `queuer` before anyone has decided what work it
becomes.

**`--tag` replaces wholesale; `--clear-tags` is how to say none.** Passing both
is refused, not ranked — two answers to one question, and the receipt would not
say which was stored. Repeating `--tag` is how a row carries several, which is
why it is declared repeatable rather than last-one-wins.

**Filtering by an unregistered tag is refused rather than answered.** An empty
list reads as "nothing is tagged that", which is exactly how a typo becomes a
wrong answer somebody acts on. `task list --tag infr` says the tag is not in the
master file and suggests `infra`.

**Retiring a tag rows still carry is refused, and says how many.** `--force`
strips it from them, and the event records the count. An operator gets the
number they need to decide, not just a no.

**Both halves land in the audit trail** (`tag_added`, `tag_removed`), because a
tag vanishing from every row it labelled is exactly the change someone will
later need explained.

**The master file is per board.** A second project starts empty rather than
inheriting a vocabulary that was never about it. `infra` on Unum and `infra` on
crm-react are different agreements, and merging them would mean one project's
rename silently rewriting another's.

### Agents are expected to tag

The rule is written into the operator's `AGENTS.md` set and the `/kb` skill: an
agent adding a row picks from `tag list`, and where nothing fits, registers the
tag it needs with a description rather than leaving the row unfiled. A registry
that only the operator writes to would be a registry that goes stale between the
work and the person, and the agent is the one who knows what the row is about.

## Consequences

Tags are a third axis beside type and lane, and the three answer different
questions: **type** is how big, **lane** is who picks it up, **tag** is what part
of the system it touches. Nothing about routing changed, so no existing board
behaves differently — the migration adds two tables and attaches to nothing.

`task list --tag` is the read this exists for. It composes with `--status`, so
`--status todo --tag queuer` is "open queuer work", which previously required
reading titles.

`attach_tags` is one query for a whole result set rather than one per row: a
board with a thousand tasks would otherwise pay a thousand round trips to render
a list.

The registry is a real cost the first time an agent reaches for a tag that does
not exist yet, and that cost is the mechanism. It is paid once per concept, by
the first agent to name it, and it is what makes `tag list` an answer instead of
a sample.

Deliberately not decided: **hierarchy.** `infra/dns` under `infra` is a real
want, and a flat vocabulary with thirteen boards may stay legible for a long
time. A prefix convention costs nothing to adopt later and cannot be taken back
once the parent/child semantics are in the schema, so the flat table ships and
the question waits for a board where flat has actually failed.

Also not decided: **renaming a tag.** Retire-and-reattach works and is explicit;
a rename would need to decide whether the old name stays readable in the event
trail, and nothing yet needs it.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — the ledger these rows belong to
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the refusal shapes reused here: name the near miss, refuse two answers to one question
- [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md) — drafts and plans, which tags apply to like anything else
- [ADR-014](ADR-014-the-repo-ships-its-own-skill-and-lives-in-the-dotfiles.md) — the `/kb` skill that documents the tagging expectation
