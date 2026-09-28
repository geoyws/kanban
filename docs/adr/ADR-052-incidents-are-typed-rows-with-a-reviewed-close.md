# ADR-052: Incidents are typed rows with a severity, a four-state lifecycle, and a reviewed close

**Status:** Proposed
**Date:** 2026-09-28
**Deciders:** George approved the slice under epic `e-c0852fe7` on 2026-09-28 (board task
`t-d94b9d1c`); he owns the scope. The wording of this document is decided by Spec-incident, the
writer of the slice; George may supersede any clause of it. Source epic `e-b481f61b` on the board
is the authority for the decisions recorded here.
**Number:** 052. ADR-051 is reserved by separate makeover work in another worktree; to avoid a
collision this decision takes the next free number, verified by directory listing at decision
time (no `ADR-051*` file exists in this checkout).

## Context

The board tracks work (tasks), decisions awaiting the operator (attention), releases
(deployments), and notes (sitreps) — but it has no record for the thing that interrupts all of
them: an outage or production breakage, its severity, what was done minute by minute, and what
was learned. Today that story lives in chat prose and in medic markdown receipts, neither of
which knows whether the incident is still open, whether anyone mitigated it, or whether a
follow-up exists. A `SEV1` at midnight is a timeline nobody can query and a close nobody reviews.

Three shapes were available:

- **Reuse attention with a new kind.** Rejected: an attention row is a decision card whose value
  is the authored question and choices ([ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md)),
  settled by the resolve ladder. An incident is the opposite shape — a severity, a one-way
  lifecycle, an append-only minute-by-minute timeline, and a close gated on a written review. A
  seventh kind would inherit a resolve permission and a composer that answer the wrong question,
  and the `COMPLAINT` slice is the precedent for when reuse is right; this is when it is not.
- **Reuse tasks with a severity tag.** Rejected: tasks are claimable work, and the one invariant
  the owner stated is that incidents are never claimed. A tag cannot enforce that — every claim,
  scheduler, and handoff path would need an incident exception, which is the silent-exception
  sprawl [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) exists to
  refuse.
- **A new typed row, `i-`, with its own lifecycle and close gate.** Decided by this record (which
  stays `Proposed` until George accepts it — see §Consequences): the id prefix makes
  the exclusion structural (claim verbs refuse what they cannot parse, as `sp-` already proves),
  and the lifecycle, timeline, links, evidence pointer, and reviewed close live in one place with
  one migration.

## Decision

Incidents are new typed rows, specified in `docs/specs/incident.md` before any implementation
(ADR-047 §6), with P1 implementation after the makeover epic `e-02a9f510`:

1. **Identity and severity.** `i-<suffix>` ids; severities `SEV1`–`SEV4`; every board. What each
   severity means is operator practice, not a documented rule.
2. **One-way lifecycle with three stamps.** `open` → `mitigated` → `resolved` → `reviewed`, one
   step at a time; `detected_at` at open, `mitigated_at` and `resolved_at` exactly once each, in
   non-decreasing order. Skipped steps are refused (spec OQ-1, closed on the conservative rule
   in `INCIDENT-02`; owner George).
3. **Append-only memory.** Timeline entries of `time`/`actor`/`text` with no edit and no delete;
   links to tasks, attention, deployments, sitreps, and hosts (hosts as opaque strings, never
   resolved); evidence as a pointer stored verbatim and never expanded, with keep-secrets-out as
   `SHOULD` practice rather than an invented enforcement gate.
4. **A close that refuses to be casual — and only geoyws performs it.** `resolved` → `reviewed`
   requires a written review naming cause, impact, what worked, and what did not, plus a follow-up
   task or an explicit recorded reason for none — each absence refused with its own sentence,
   fail-closed — and the transition
   itself is effected only by geoyws (OQ-3 verdict `a-915fa4cc`, choice owner, approve, 2026-09-28:
   only geoyws marks an incident reviewed; lane agents author the review text and file or propose
   the follow-up — spec `INCIDENT-15`). Any other actor attempting it is refused and writes nothing.
5. **No claims, served reads, Needs-you signal.** Claim and lease verbs refuse `i-` ids; the
   browser gains read-only `GET /incidents` and `GET /incident/<board>/<id>` with a JSON
   projection answering the same rows to the same principal under the existing board/tag
   authorization (non-enumerating refusal); open `SEV1`/`SEV2`
   rows join the Needs-you queue; medic markdown receipts keep working byte-identically and are
   accepted as evidence pointers.
6. **One migration.** Forward-only from V33 to the next free `BOARD_Vnn` after integration, with
   empty incident storage; existing rows untouched. No version is pinned here: sibling slices
   (tasks `t-14` and `t-2cff`) race for V34, so the landing change names the first free version
   after the other lands.

The CLI argv is pinned in the specification (spec OQ-2, closed); the lifecycle permission model
(spec OQ-3, board attention `a-915fa4cc`, closed 2026-09-28 by George's verdict — choice owner,
approve: only geoyws marks an incident reviewed, lane agents author the review text and the
follow-up, spec `INCIDENT-15`) is decided and blocks nothing further: the functional writes are
specified with the acting actor recorded, and the reviewed transition runs as geoyws.

## Consequences

An outage becomes queryable: severity, status, timeline, links, review, and follow-up in one row
instead of scattered prose. The cost is a fourth core row type beside tasks, attention, and
sitreps, with its own migration and its own verbs — borne because neither existing type can
carry the lifecycle without inheriting the wrong semantics.

The reviewed close is a real gate, not a suggestion: a `resolved` incident with no review stays
`resolved` and says why, in the [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
form where a refusal is also its own fix. Lanes that want a green board must write the review
and file or disclaim the follow-up.

What a future change must update: `docs/specs/incident.md` (with visible supersession per
ADR-047 §3, never a silent rewrite), its rows in
`docs/testing/compiled-rust-e2e-matrix.md`, and this ADR by supersession if the decision itself
moves. Implementation lands under a P1 task after `e-02a9f510`; until George accepts this record
it stays `Proposed` and claims nothing further.

## References

- Board task `t-d94b9d1c` (the slice row under epic `e-c0852fe7`); source epic `e-b481f61b`;
  makeover epic `e-02a9f510` (implementation sequencing); George's 2026-09-28 approval and
  decisions, taken from the delegating lane contract (this worktree has no KB access — an
  independent reviewer confirms the quotations).
- `docs/specs/incident.md` — the requirement-level specification of this decision.
- [ADR-047](ADR-047-kanban-adopts-specification-driven-development.md) — why the spec precedes
  implementation (§6), why no budget is invented (§9), and why IDs outlive their wording (§3).
- [ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) — why attention
  reuse was rejected.
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — why the gates
  refuse with sentences that name the fix.
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) — why the new pages add no browser
  write verb.
