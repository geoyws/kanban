# ADR-055: Lanes send addressed durable messages — pull delivery, pointer wakes, piggybacked notices

**Status:** Proposed
**Date:** 2026-09-29
**Deciders:** George
**Sources:** kb `e-ea261014` (George 2026-09-29 decisions); planner decisions via
the delegating contract for task `t-bf2458c3`; slice approval George 2026-09-29
in conversation per `e-ea261014`.

## Context

Lane-to-lane traffic today has no durable addressed form. Upward lanes are
sitreps (broadcast, not addressed), attention (George-decision-only by design),
and send-keys (not durable). A lane that needs a named party to see something —
a question for the superdriver, a request to another lane, a blocker for the
lead — either shouts it in a sitrep nobody owes a read, spends George's
attention budget on what is not a George decision, or sends keys into a pane
that may not be listening. The inbox side is landing separately: the steered
watch stream (`watch --lane`/`--note-kind`, row `t-fde5d91c`) gives every lane a
stream worth extending, and the dispatcher already knows how to wake an idle
coordinator — it just has no pointer-sized thing to wake it with.

Work is specified in `docs/specs/outbox.md` (slice `OB`, `OB-01` .. `OB-21`).

## Decision

Addressed durable messages with pull delivery — five parts:

1. **Addressed `m-` rows, four kinds, threads as rows** (`OB-01` .. `OB-07`).
   Every message runs from a lane actor (`@:team/board/lane`, the handoff
   `--to` grammar) to one named recipient — a lane superdriver, a lane's own
   planner pane, another lane, the team lead, or a named registered role —
   resolved against a registry beside the rules. Kind is exactly
   `question`/`request`/`report`/`blocker`; bodies are bounded (bound OQ-1);
   an optional same-board task link is allowed; replies are `m-` rows naming
   the original and landing in the original sender's inbox. Status moves
   `open` → `acked` → `answered`|`closed` with the recipient owning ack/answer
   and either side owning close. Unread means not-yet-acked and repeats on
   every carrier until acked. Events are hash-chained (ADR-029).
2. **Sender-board storage, cross-board gather, no oracle** (`OB-09`,
   `OB-10`). The row lives on the sender's board under that board's tag authz
   and nowhere else — no fan-out copies. Queue reads gather from every board
   the caller may read and silently skip the rest, with the non-enumerating
   denial (`rust/authz.rs:34`) reused at queue scale: denied rows are
   indistinguishable from nonexistent ones.
3. **Pull delivery with pointer wakes and piggybacked notices** (`OB-11`,
   `OB-13` .. `OB-15`). The board stays the record; sending writes the row
   and nothing else. A dispatcher subscription may wake an idle coordinator
   with the `m-` id only — never content. Every kb call that knows its caller
   (`--as` or `KANBAN_ACTOR`) carries a compact unread notice on object
   outputs (top-level `mail`: ids, senders, kinds, one-line headlines, at most
   five plus a remaining count, never bodies; array listings byte-identical),
   and the watch heartbeat is the reliable carrier for idle lanes.
4. **`watch --to` extends WATCH, not a second dialect** (`OB-12`). The inbox
   mirrors onto the steered watch stream under the same cursor-binding,
   envelope, redaction, and heartbeat rules (`WATCH-01`..`WATCH-12`), landing
   with row `t-fde5d91c` and specified once, here.
5. **Guardrails as requirements** (`OB-16` .. `OB-19`). Attention stays
   George-decision-only: a reply is never an owner verdict and never resolves
   attention (the resolve path refuses `m-` ids). No secrets by practice
   (`SHOULD`, no detector built or claimed). Messages never carry leases and
   never move claims. Pane wakes follow `/pane-agent`: capture-before-send,
   idle-only unless George says otherwise.

Settled rows archive out of the operational queue (ADR-021), listings cap and
refuse like every other listing (ADR-037), and the verbs ride the generated
MCP surface with no new trust boundary (`OB-20`, `OB-21`). No web panel:
serve is retired under ADR-053.

## Alternatives considered

- **Sitreps-only (broadcast covers it).** Rejected: sitreps address nobody, so
  nobody owes a read — the per-recipient queue with its unanswered-N view is
  the whole point. Broadcast stays for genuinely public traffic; addressed
  traffic gets a row, a recipient, and a lifecycle.
- **Attention-for-all (raise a card per message).** Rejected: attention is
  George's decision queue, and every message-as-card spends the scarcest
  budget on the board — George's eyes — on traffic that is not a George
  decision. `OB-16` holds the line structurally: messages cannot resolve
  attention even when they read like decisions.
- **A second stream dialect (a dedicated inbox stream).** Rejected: two cursors,
  two envelopes, and two binding rules for one lane's "what changed for me"
  doubles the surface `t-fde5d91c` is building and strands every existing
  consumer. `--to` rides the lane-binding, redaction, and heartbeat machinery
  that already exists; any stream/queue deviation is specified as a failure,
  not a dialect.

## Consequences

- The slice adds a message table plus chained events (board-schema migration
  to the next free version above 36), the recipient registry beside the rules,
  `watch --to` (with `t-fde5d91c`), dispatcher pointer wakes, and the `mail`
  notice on qualifying outputs — CLI and MCP alike, no web surface.
- Overlap-check stays the refusal owner: a message neither preempts nor
  re-authorises a refused filing on another lane's board.
- Open questions to George (body bound, exact argv, seed roles, `--to`
  scheduling) arrive as a card — filed by the requester, not by this ADR — and
  gate implementation, not the PROPOSAL stamp.
- History stays readable: sitreps, attention, handoffs, and claims are
  untouched in meaning; messages link to tasks and debate attention outcomes
  without ever becoming either.
