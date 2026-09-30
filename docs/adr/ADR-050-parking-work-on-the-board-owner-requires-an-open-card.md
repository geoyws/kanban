# ADR-050: Parking work on the board owner requires an open card

**Status:** Accepted
**Date:** 2026-09-20
**Deciders:** claude@driver, implementing rule `g-74e8d80c` filed on the acies board as t-d8e5f023, under George's standing goal to finish the board's tasks. George did not rule on the mechanism himself and may supersede it by a later ADR.

## Context

The attention card is the only surface that reaches the board owner. A lane
that stops, writes `checkpoint --state blocked` with "George re-logins to
bootstrap" as its next action and raises nothing has recorded the dependency
where only another agent reading that task will ever see it — and the task
leaves the claimable queue, so the lane reads as having nothing to do while the
work is in fact waiting on a person who was never told.

That is not hypothetical. It was measured twice on the acies board on
2026-09-17: t-336e6de2 note 198 and t-7589b01e checkpoint 105, both parking
work on George with no open `a-*` row, both discovered only because someone
went looking at a queue that said empty.

Rule `g-74e8d80c` already says to raise the card. A rule in a registry that
the write path does not enforce is a rule that gets followed until the
inconvenient moment, which is exactly the moment it exists for.

Two shapes were available:

- **Auto-raise.** The blocked write creates the card itself. Rejected: the
  card's whole value is the authored question, context and choices
  ([ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md)),
  and a machine-composed card from a next-action sentence is the low-quality
  row that trains the operator to skim the deck.
- **Refuse, naming the fix.** The write that would strand the work fails and
  says what to raise, in the [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
  form where a refusal is also its own fix. This is the precedent the tree
  already carries for a law of this kind
  ([ADR-037](ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md)).

## Decision

Refuse, in the store, beside the write.

1. **One gate, both writes.** `require_owner_has_a_card` in `rust/store.rs` is
   called from `Store::checkpoint` and `Store::create_handoff`, inside each
   method's existing transaction and before any row lands, so a refusal leaves
   no half-written record and a card raised earlier in the same batch counts.

2. **Only the fields that assign.** A blocked checkpoint's `--next-action` and
   either record's `--blocker` values are read. Those two fields *are* the
   assignment: what happens next, and what stops it. The summary is narrative
   and routinely cites him — "per George's 2026-09-01 decision" — and reading
   it would refuse records that park nothing on anyone. A false refusal on an
   honest record is the expensive error here, so the cut is deliberately
   conservative and the summary is out.

3. **Two literals, one of them sourced.** `OPERATOR_ACTOR` (`rust/model.rs`)
   is the real source for `geoyws`: he is the one actor who may settle any
   attention row, which is what makes him the one actor a blocked record can be
   waiting for. His given name has no source at all — no table records that
   `geoyws` is George. `board_meta` holds the board's name and its audit chain;
   the registry's `boards` and `workspace_roots` rows hold paths and
   timestamps; `principals` are the *caller's* frozen identity
   ([ADR-038](ADR-038-linux-principal-broker-policy-and-bootstrap.md)), not the
   board's owner. So `George` is a literal, capital-G only, matched standalone:
   `Georgetown` and `geoywsMBP` are not him. Nor is an actor, lane or path
   compound: this board addresses its lanes as `@:geoyws/kanban/driver`, and a
   next action handing the step to that LANE is work being assigned to a
   worker, not parked on a person. A separator touching the match — `:` before
   it, or `/` on either side — is what tells a path from a name.

4. **The refusal quotes the clause.** Not the whole field and not a
   restatement: the sentence or clause that matched, so a false positive costs
   one rewrite instead of a hunt, followed by the command that resolves it —
   `kb attention raise "<the ask>" --as <agent> --kind blocking --task <id>`.
   A clause carrying its own double quote is echoed with single quotes, so the
   sentence keeps exactly one quoted span rather than breaking in half.

5. **Scope.** Only blocked checkpoints and handoff blockers. A `continue` or
   `done` checkpoint claims work happened and parks nothing; a session handoff
   names no task, so there is no row for a card to hang on and nothing to
   require; and no free-prose scan of notes, sitreps or summaries is added.

## Consequences

A lane that must stop on the operator now does two writes instead of one, in
the order that leaves the board readable: raise the card, then checkpoint. The
card is what makes the wait visible in `kb att list` and in the operator deck,
which is the whole point — the blocked checkpoint records the state, the card
recruits the person.

A record that names him without assigning him anything is untouched, and that
is asserted rather than hoped: the compiled-process case
`a_blocked_checkpoint_that_assigns_the_owner_nothing_is_written_without_a_card`
writes all three such records on a cardless board — one parking the work on
nobody, one citing a past decision of his, one handing the step to the
`@:geoyws/kanban/driver` lane — and requires every one of them to land.

Because the match is two literals, a lane can evade the gate by writing "the
operator" or "geo". That is accepted. This gate is aimed at the shape that was
actually measured, not at an adversary; widening it to prose would trade the
false negative for the false refusal, which is the worse of the two on a
surface whose refusals must be believed.

## References

- `rust/store.rs` — `OWNER_NAMES`, `owner_clause`, `has_open_attention`, `require_owner_has_a_card`; called from `Store::checkpoint` and `Store::create_handoff`
- `rust/model.rs` — `OPERATOR_ACTOR`
- `tests/e2e.rs` — `a_blocked_checkpoint_that_parks_work_on_the_owner_needs_a_card_first`, `a_blocked_checkpoint_that_assigns_the_owner_nothing_is_written_without_a_card`, `a_handoff_blocker_that_parks_work_on_the_owner_needs_a_card_first`
- `docs/testing/compiled-rust-e2e-matrix.md` — the three owner-gate rows
- ACIES board: rule `g-74e8d80c`; task t-d8e5f023; measured on t-336e6de2 note 198 and t-7589b01e checkpoint 105
