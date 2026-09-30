# Specification: lanes send addressed durable messages with pull delivery, pointer wakes, and piggybacked unread notices (slice OB)

## 1. Identity and baseline

- **Slice ID:** `OB`. Requirement IDs are `OB-01` .. `OB-21`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-29` at commit `2752a17` on branch `wt/t-bf2458c3-spec`.
  Board schema at the baseline is `36` (`rust/db.rs:2920`), and no message surface
  exists yet: no `m-` id prefix, no `message` subcommand in the `COMMANDS` table
  (`rust/lib.rs:124`-`rust/lib.rs:292`), no `watch --to` flag, and no `mail` field
  on any CLI/MCP output. The specification is written before the implementation,
  as ADR-047 §6 requires (per `docs/specs/README.md:15-22`).
- **Status:** `SPEC-READY` on 2026-09-29 (independent reviewer applying the SDD §1 exit criteria:
  one P3 finding, fixed in-tree with the 160-character headline bound. Specification readiness
  only — it authorises neither implementation, nor rollout, nor release).
- **Owner (product scope):** George. He alone resolves scope, the open questions
  in §7, and whether a non-goal in §2 is reinstated.
- **Decider (wording of this document):** OutboxSpec, the writer of this slice.
  Where this document and the delegating contract differ on a fact, the contract
  wins and this document is corrected (see the closing note in §7).
- **Sources:**
  - **George, 2026-09-29** (kb `e-ea261014` body plus planner decisions, via the
    delegating contract for task `t-bf2458c3`): addressed `m-` rows from a lane
    actor to a named recipient; kinds question/request/report/blocker; optional
    task link; body; thread replies as `m-` rows that land in the sender inbox;
    status `open` → `acked` → `answered`|`closed`; durable hash-chained events;
    per-recipient queue with unanswered-for-N-minutes; sender-board storage under
    board tag authz with cross-board gather and no existence oracle; pull plus
    optional wake; `watch --to` as a WATCH extension; dispatcher pointer wakes;
    per-call unread-mail piggyback with heartbeat as the reliable carrier;
    attention stays George-decision-only; no secrets; messages never replace
    handoffs/claim transfers; pane wake follows `/pane-agent`. Recorded here as
    decided inputs; not relitigated.
  - Slice approval: **George 2026-09-29 in conversation** per `e-ea261014` (no
    separate slice row under epic `e-c0852fe7` is needed).
  - `docs/adr/ADR-055-lane-outbox-addressed-messages.md` — the decision this
    specification implements.
  - `docs/specs/watch.md` (slice `WATCH`) — the inbox steered watch stream this
    slice extends with `--to`; `watch --lane`/`--note-kind` cursor binding and
    the one-dialect rule (`WATCH-02`, `WATCH-07`) are reused, not re-specified.
  - Implementing rows: lane inbox `t-fde5d91c` (WATCH extension coordination)
    and this slice's own implementation row (unnamed at write time — §8 names
    `planned` tests only).
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the
    conventions this document is written under; §6 is why this slice is specified
    before implementation, §9 is why §6 carries observations, not budgets.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` —
    why unknown kinds, illegal transitions, and unaddressed sends are refused
    with a sentence that names the accepted values, and why a refusal writes
    nothing.
  - `docs/adr/ADR-027-rules-are-one-tag-scoped-kb-document.md` — precedent for
    piggybacking rules only at claim/resume; this slice's per-call mail notice
    follows the same inject-only-where-known pattern.
  - `docs/adr/ADR-021-settled-history-leaves-operational-indexes.md` — why
    settled messages archive out of the operational queue.
  - `docs/adr/ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md` —
    why listings are capped and refuse rather than silently truncate.
  - `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` —
    why message events are hash-chained.
  - `docs/adr/ADR-053-the-web-view-is-retired.md` — why every surface below is
    CLI/MCP: the serve layer is deleted with no replacement UI, so no web panel
    is specified, no served-bytes evidence is owed, and every `Layer` below is
    `process` (no `chrome`, no `http`).
  - Shipped surface at the baseline: `BOARD_SCHEMA_VERSION` and
    `BOARD_MIGRATIONS` (`rust/db.rs:2920`, `rust/db.rs:3479`-`rust/db.rs:3482`);
    the tag-authorization non-enumerating denial (`rust/authz.rs:34`); the
    handoff address grammar (`--to @:team/project/LANE`, `rust/store.rs:8003`-
    `rust/store.rs:8022`); the `handoff list --to AGENT` precedent
    (`rust/lib.rs:179`); the claim-sweep read path
    (`rust/store.rs:3536`-`rust/store.rs:3538`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the
  slice's evidence table; the matrix section
  `## Requirements trace — docs/specs/outbox.md` is the trace of record when it
  lands (convention at `docs/testing/compiled-rust-e2e-matrix.md:224-236`).

## 2. Purpose and scope

**Intended outcome.** A lane asks, requests, reports, or flags-to another named
party once, on the record, and the recipient finds it by pulling an inbox —
with a pointer-wake when idle and a compact unread notice on calls they already
make — without a second stream dialect, without attention inflation, and without
moving the board of record.

**Users / actors.**

- **Lane agents** (drivers, superdriver, planner panes) — send addressed
  messages, pull their inbox, ack, answer, and close from the CLI/MCP.
- **The team lead / named registered roles** — receive and answer messages
  addressed to the role, same verbs.
- **George** — owns attention (unchanged), resolves the open questions in §7,
  and approves the slice (done, `e-ea261014`).
- **Adapter clients (MCP) and the dispatcher** — read queues through the
  generated surface; wake idle lanes with pointers, never content.

**In scope.** The `m-` message identity and address grammar; the four kinds;
body bounds; optional task link; thread linkage; the four-state lifecycle with
its actor gates; the per-recipient queue with the unanswered-N view; sender-
board storage with cross-board gather and no-oracle reads; pull delivery; the
`watch --to` WATCH extension; dispatcher pointer wakes; the per-call mail
notice with heartbeat as carrier; archival and capped listings; MCP tools; the
epic guardrails as requirements.

**Boundaries.**

- **Inbox today is the steered watch stream** (`watch --lane`/`--note-kind`,
  row `t-fde5d91c`): this slice extends that stream with `--to`, it does not
  build a second inbox. Upward lanes today are sitreps (broadcast), attention
  (George only), and send-keys (not durable) — all untouched.
- **Attention (ADR-042) is linked, never driven, by messages.** A coordinator
  reply is never an owner verdict and never resolves attention (`OB-16`).
- **Overlap-check stays the refusal owner.** An addressed message never
  overrides, preempts, or stands in for an overlap refusal on another lane's
  board.
- **Pane wake follows `/pane-agent`.** Capture-before-send, idle-only unless
  George says otherwise (`OB-19`); this slice adds no wake mechanism of its own
  beyond the pointer contract (`OB-13`).
- **The CLI/MCP grammar is proposed in §5, not pinned in §3.** Exact argv and
  the body byte bound are open questions for George (OQ-1, OQ-2); the refusal
  sentences in §3 are what the product says regardless of the argv that reaches
  them.

**Non-goals.**

- **No web panel.** Serve is retired (ADR-053); there is no page, no socket, no
  served-bytes evidence.
- **No push delivery with content.** Delivery is pull; wakes carry pointers
  only. A content-bearing push is a different slice and arrives as one, never as
  a reinterpretation.
- **No replacement for handoffs or claim transfers.** Messages never carry
  leases and never move claims (`OB-18`).
- **Any performance, availability, latency, or retention commitment:** §6 cites
  observations only; the unanswered-N threshold is a caller-supplied filter, not
  a delivery SLA.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that will prove the
requirement: `process` — a compiled-binary process-boundary exchange, no HTTP
and no browser (this slice ships no pages, so there is no browser surface to
drive).

Refusal sentences below are quoted verbatim, with `{value}` and `{id}` as the
substitution points. Every refusal exits non-zero and writes nothing — no row,
no event, no wake, no piggyback side effect (ADR-008 fail-closed).

### Address, kinds, body, links, threads

**OB-01** — Address every message from a lane actor to a named recipient.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (addressed
`m-` row, `@:team/board/lane` sender).
`A message row carries an id of the form `m-<suffix>` (lowercase `m`, a hyphen,
a non-empty suffix of letters, digits, dot, underscore, or hyphen — the same
shape the `sp-` precedent fixes at `rust/model.rs:972`), a sender that is the
calling lane's full typed actor (`@:team/board/lane`, the handoff `--to`
grammar at `rust/store.rs:8003`-`rust/store.rs:8022`), and exactly one named
recipient. The recipient registry lives beside the rules (one tag-scoped
kb document per ADR-027): the send path resolves the name against that
registry, and an unresolvable recipient is refused with
`unknown message recipient {value}; expected one of {names}`. A send with no
recipient is refused with `a message needs an addressee: pass --to NAME`.
The id is operator-suppliable and otherwise minted as `m-` plus eight hex
characters. An id of any other shape is refused with `message id must start
with m-, include a suffix, be at most 64 ASCII characters, and contain only
letters, digits, dot, underscore, or hyphen`; a send naming an id that already
exists on the board is refused with `message {id} already exists`.`
`Permissions: the sender is the acting `--as` caller; `--as` naming a lane
other than the caller's own is refused under the board's existing actor rules.`

**OB-02** — Restrict recipients to the lane set, the lead, and registered roles.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (recipient set).
`The recipient registry admits exactly: a lane superdriver; a lane's own
planner pane; another lane (including a lane on another board); the team lead;
a named registered role. A recipient outside these classes is refused with
`{value} cannot receive lane messages; expected a lane, the lead, or a
registered role`. An addressed message never overrides an overlap refusal:
where overlap-check on the recipient's board refuses a filing, the refusal
stands and the message neither preempts it nor re-authorises the filing.`

**OB-03** — Carry exactly one of four kinds.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (message kinds).
`Every message carries exactly one kind: `question`, `request`, `report`, or
`blocker`. Any other kind is refused with
`invalid message kind {value}; expected question, request, report, or blocker`.
The kind is written once at send and never edited afterwards.`

**OB-04** — Bound message bodies; refuse over the bound.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (body bounds;
bound value OQ-1).
`A message body is bounded text. A send whose body exceeds the bound (OQ-1) is
refused with `message body exceeds {limit} bytes` and writes nothing. Bodies
are stored verbatim within the bound; there is no edit verb — a correction is
a reply (`OB-05`).`

**OB-05** — Link an optional task; thread replies as rows into the sender inbox.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (task link,
thread).
`A message MAY carry one task link naming a task id on the sender board; a
link naming a task that does not exist there is refused, quoting the task id,
and writes nothing. A reply is itself an `m-` row carrying the original's id;
the reply is addressed to the original's sender and lands in that sender's
inbox queue (`OB-08`), so a thread is rows, not edits. A reply naming an
original that does not exist, or that the replier may not read, is refused
with the existing non-enumerating `denied or not found` (`rust/authz.rs:34`)
rather than a shape that distinguishes unknown from unreadable.`

### Lifecycle and queues

**OB-06** — Move `open` → `acked` → `answered`|`closed`, gated by role.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (status
machine).
`The only status values are `open`, `acked`, `answered`, `closed`, and the only
moves are `open` → `acked` (ack), `acked` → `answered` (answer), and
`open`|`acked`|`answered` → `closed` (close). Any other target value is refused
with `invalid message status {value}; expected open, acked, answered, or
closed`; any other move is refused with
`message {id} is {status} and cannot move to {target}`, and the row keeps its
status. Only the named recipient identity effects `open` → `acked` and
`acked` → `answered`; only the sender or the recipient effects a close; any
other actor is refused with `message {id} is addressed to {recipient}` and
writes nothing.`

**OB-07** — Treat unread as not-yet-acked and repeat it.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (unread
semantics).
`Unread means not-yet-acked: a message counts unread until its recipient acks
it (`OB-06`), and every unread carrier — the inbox queue, the `watch --to`
stream, the per-call notice (`OB-14`) — repeats it until then. Answering
without acking is impossible: the `acked` step is never skipped.`

**OB-08** — Serve a per-recipient queue with an unanswered-N view.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (queue,
unanswered-for-N-minutes).
`The inbox verb answers, for the calling identity, that identity's messages:
open and acked rows first, each row carrying id, sender, kind, status, and
headline — never the body. The caller-supplied `N` filter (`--unanswered N`)
restricts to rows still unanswered after N minutes; N is a display filter, not
a delivery promise, and the default listing carries no N claim. The headline
is the body's first line truncated to 160 characters, so five headlines stay
well under a kilobyte combined; bodies are fetched per-id only.`

### Storage and cross-board reads

**OB-09** — Store the message on the sender board under board tag authz.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (sender-board
storage).
`The message row and its hash-chained events (ADR-029) live on the sender's
board and nowhere else: no copy is fanned out to the recipient's board. The
sender board's tag authorization governs the write; a caller who may not write
there receives the board's existing refusal and writes nothing. The board
stays the record: delivery, wakes, and notices are views over it, never
second stores.`

**OB-10** — Gather cross-board queues with no existence oracle.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (cross-board
gather, no oracle).
`A queue read gathers from every board the caller may read and silently skips
the rest: rows on boards the caller may not read are absent from the result,
and no error, count, timing shape, or refusal sentence distinguishes a denied
row from a nonexistent one — the tag-authorization non-enumerating denial
(`rust/authz.rs:34`) reused for reads at queue scale. Cursor binding for the
steered stream reuses the WATCH lane-binding rule (`WATCH-02`): reusing a
persisted cursor with a different normalized recipient set fails closed before
any row is read.`

### Delivery: pull, watch extension, pointer wakes, piggyback

**OB-11** — Deliver by pull; the board stays the record.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (pull plus
optional wake).
`Delivery is pull: a message is delivered when its recipient's queue, stream,
or notice shows it. Sending writes the row and its events and nothing else —
no push, no content fan-out, no second store. A wake (`OB-13`) or notice
(`OB-14`) that never arrives changes nothing about what the queue holds.`

**OB-12** — Mirror the lane inbox on `watch --to` as a WATCH extension.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (`watch --to`,
coordination with `t-fde5d91c`).
``watch --to ME` (repeatable, one identity per flag, ORed within the family,
ANDed with every other predicate family) mirrors the caller's lane inbox over
the steered watch stream: the same rows `OB-08` would list, as events, under
the same cursor-binding, envelope, redaction, and heartbeat rules as
`WATCH-01`..`WATCH-12`. There is no second cursor dialect and no second
envelope: the recipient set rides the bound predicate set exactly like the
lane set (`WATCH-02`), and any deviation between the stream and the queue for
the same identity is a failure of this requirement. The flag lands with the
WATCH implementation (`t-fde5d91c`) and is specified here once, not twice.`

**OB-13** — Wake idle lanes with a pointer, never content.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (dispatcher
pointer wake).
`A dispatcher subscription MAY wake an idle coordinator with a pointer: the
`m-` id only, never the body, the headline, the kind, or any metadata beyond
routing. Content arrives only when the woken lane pulls its queue (`OB-08`)
under its own read authority — a wake grants nothing and a denied row stays
denied (`OB-10`). A wake names no row the woken identity may not read; where
no addressed row is readable, no wake fires.`

**OB-14** — Piggyback a compact unread-mail notice on calls that know the caller.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (piggyback
rules a–d; ADR-027 precedent).
`Every kb call that knows its caller carries a compact unread-mail notice:
(a) identity comes from `--as` or `KANBAN_ACTOR`, default in that order — a
call with no identity carries no notice; (b) object outputs gain a top-level
`mail` field while array listings are byte-identical to today; (c) the notice
carries id, sender, kind, and one-line headline per message, at most five plus
a remaining count — never bodies; (d) unread (`OB-07`) repeats on every
qualifying call until acked. The notice is a view: rendering it reads nothing
the caller may not read and writes nothing.`

**OB-15** — Carry the reliable notice on the heartbeat.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (heartbeat
carrier).
`The watch heartbeat is the reliable carrier for the unread notice: an idle
`--follow` stream's heartbeat frames carry the same compact notice as
`OB-14` (at most five plus a remaining count, never bodies), so a lane holding
only a stream still learns of mail without polling. Per-call notices
(`OB-14`) are best-effort beside it; the heartbeat is the one the slice
promises.`

### Guardrails (requirements, not wishes)

**OB-16** — Never let a message reply become an owner verdict or resolve attention.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (attention
stays George-decision-only).
`Attention stays George-decision-only: a message reply is never an owner
verdict and never resolves attention. Pointing an attention resolve/approve
path at an `m-` id — or replaying message text as the deciding note — is
refused, quoting the id, and the attention row keeps its status. A coordinator
answer (`OB-06`) that reads like a decision is delivered as correspondence,
not as authority: it moves the message to `answered` and nothing else.`

**OB-17** — Keep secrets out of messages.
Strength: `SHOULD` · Layer: `process` · Source: George 2026-09-29 (no
secrets).
`Secret material SHOULD NOT be pasted into any message field; a pointer (task
id, receipt path, row id) SHOULD name where the evidence lives instead. This
is operator practice, not an enforced gate: enforcement would need a secret
detector, which this slice does not build (ADR-047 §9 forbids inventing the
commitment). A `SHOULD` exception records its reason; it does not fail the
gate. Redaction before emission (`WATCH-10`) still applies to every envelope
and notice this slice renders.`

**OB-18** — Never replace handoffs or claim transfers.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (handoffs and
claims untouched).
`Messages never carry leases and never move claims: no claim, lease,
handoff-create, or handoff-accept verb takes an `m-` id, and pointing one at a
message is refused, quoting the id, leaving the message and the claimable
queue untouched. Messages never appear in claim candidates and never carry a
lease token. The claimable queue is exactly what it was at the baseline for
every board.`

**OB-19** — Wake panes by `/pane-agent` rules only.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (pane wake
discipline).
`A pointer wake that reaches a pane follows the `/pane-agent` discipline:
capture-before-send, and idle panes only unless George says otherwise in the
message thread or on the board. A wake that would interrupt a non-idle pane
without that authority is not sent; the queue (`OB-08`) holds the message
until the lane pulls.`

### Retention, caps, tools

**OB-20** — Archive settled messages; cap listings like every other listing.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (archival
ADR-021, caps ADR-037).
`Settled messages (`answered`, `closed`) leave the operational queue under the
ADR-021 rule — archived out, indexed for lookup, never deleted — while `open`
and `acked` rows stay operational. Every listing verb carries an explicit
limit and refuses a default-limit breach exactly like every other listing
(ADR-037): no silent truncation, no unbounded dump.`

**OB-21** — Expose the surface on MCP with no new trust boundary.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29 (MCP tools).
`Send, inbox (with the N filter), ack, answer, and close are exposed through
the generated MCP surface beside the CLI, under the same actor, authz, refusal,
and no-oracle rules as the CLI verbs above. No new trust boundary is added:
adapter clients read what the calling identity may read and write what it may
write, nothing more.`

## 4. Acceptance examples

Concurrency needs no example: sending appends rows, queue reads select them,
and two recipients' queues share no mutable state. Unauthorized access is
read-authority gating plus actor-gated moves (`OB-06`, `OB-10`), not a write
refusal on someone else's row.

### A1 (OB-01, OB-02, OB-03, OB-04)

*Given* a board with lanes `driver-2` and `driver-3` and a recipient registry
naming `@:team/proj/driver-3`,
*when* `@:team/proj/driver-2` sends a `question` with a short body to that
recipient,
*then* the row lands as `m-<suffix>` with that sender, recipient, and kind at
status `open`; and *when* the sender names kind `urgent`, no recipient, or a
body over the OQ-1 bound, *then* each send is refused with the `invalid
message kind`, `needs an addressee`, or `exceeds` sentence and writes nothing.

### A2 (OB-05, OB-06, OB-07)

*Given* the `m-` row from A1 at `open`,
*when* the recipient pulls the inbox, acks, and answers with a reply row,
*then* the original moves `open` → `acked` → `answered`, the reply is its own
`m-` row addressed to the original sender and appears in that sender's inbox;
and *when* a third lane tries to ack the original, *then* the move is refused
with the `addressed to` sentence and the status does not move.

### A3 (OB-08, OB-10 — the oracle path)

*Given* two boards, each holding a message for the same caller identity, where
the caller may read only the first board,
*when* the caller pulls the queue,
*then* the result holds the first board's rows with id, sender, kind, status,
and headline — no bodies — and nothing in the output (no error, no count, no
sentence) distinguishes the second board's denied rows from nonexistence; and
*when* the caller passes `--unanswered 30`, *then* only rows still unanswered
after 30 minutes remain.

### A4 (OB-09, OB-11, OB-13 — the wake path)

*Given* a sent message its recipient has never pulled,
*when* the dispatcher subscription fires,
*then* the idle coordinator receives the `m-` id and nothing else — no body,
no headline, no kind; and *when* the woken lane pulls its queue, *then* the
full row arrives under its own read authority, while a lane that may not read
the sender board receives no wake at all.

### A5 (OB-12, OB-15 — the stream path)

*Given* a lane holding one `watch --follow` stream with `--to` naming its own
identity,
*when* a message is sent to that lane and no other event fires,
*then* the next heartbeat frame carries the compact notice (id, sender, kind,
headline, at most five plus a remaining count) under the same cursor, envelope,
and redaction rules as any steered stream; and *when* the same cursor string
is replayed with a different `--to` set, *then* the replay fails closed before
any row is read.

### A6 (OB-14 — the piggyback path)

*Given* a lane with two unread messages,
*when* it runs an object-output call with `--as` naming itself and then an
array-listing call,
*then* the object output gains a top-level `mail` field naming both (ids,
senders, kinds, headlines, no bodies) while the array listing is byte-identical
to its pre-slice shape; and *when* it calls with no `--as` and no
`KANBAN_ACTOR`, *then* no notice is attached.

### A7 (OB-16, OB-17, OB-18, OB-19 — the guardrail path)

*Given* an open attention card and a message thread debating its outcome,
*when* the coordinator answers the message with text that reads like a
decision and then points the attention resolve path at the `m-` id,
*then* the message moves to `answered`, the resolve is refused quoting the id,
and the attention row keeps its status; and *when* a claim verb is pointed at
an `m-` id, *then* it is refused quoting the id with the claimable queue
unchanged; and wakes to a non-idle pane fire only with George's authority on
the record, otherwise the queue holds the message.

## 5. Contracts and data

- **Interface version or schema:** the proposed CLI grammar (exact argv OQ-2):
  `message send --to NAME --kind question|request|report|blocker --as ACTOR
  --body TEXT [--task ID] [--in-reply-to m-…] [--id m-…] [--json]`;
  `message inbox --as ACTOR [--unanswered N] [--limit N] [--json]`;
  `message show m-… --as ACTOR [--json]` (the only verb that returns a body);
  `message ack|answer|close m-… --as ACTOR [--body TEXT] [--json]`
  (`--body` on answer only); `watch --to IDENTITY` (repeatable) on board/task
  scope, landing with `t-fde5d91c`. Object outputs gain top-level `mail`
  (`OB-14`); array listings are unchanged. MCP exposes the same five verbs
  (`OB-21`). Errors and diagnostics stay on stderr; envelopes and rows on
  stdout only.
- **Data invariants:** one row per message, id unique per board; kind and body
  immutable after send; status moves only along `OB-06`; a reply always names
  an existing readable original; every send/move appends a hash-chained event
  (ADR-029); unread equals not-yet-acked (`OB-07`); wakes and notices are views
  and hold no rows.
- **The `m-` schema:** `id` (`m-` + suffix, §3 shape), `from` (full lane
  actor), `to` (resolved recipient name plus the registry class from `OB-02`),
  `kind` (four values), `task` (nullable, same-board), `in_reply_to`
  (nullable `m-` id), `body` (bounded text, OQ-1), `headline` (derived first
  line, never stored-secret-adjacent), `status` (four values), `created_at`,
  `acked_at`/`answered_at`/`closed_at` (each set exactly once, by its own move,
  never edited after), `events` (append-only, hash-chained).
- **Migration:** one board-schema migration to the next free version above 36,
  adding the message table, its event chain, and the recipient registry
  pointer; direction is forward-only with the version bump committing only with
  the step, so a failed step leaves the board at its prior `user_version` with
  prior data intact and a re-run retries cleanly. Pre-existing boards gain an
  empty queue — no backfill.
- **Compatibility:** pre-slice binaries never see `m-` rows (new table, new
  verbs); array CLI/MCP outputs are byte-identical (`OB-14b`); cursors from
  before `--to` resume with the recipient family empty (the `WATCH-08`
  `#[serde(default)]` convention), and newer cursors fail closed on older
  binaries rather than dropping the recipient set.
- **Ownership:** the sender board owns the row and its events; the recipient
  registry lives beside the rules (ADR-027); the recipient's board owns
  nothing of the message; the dispatcher owns subscriptions, not rows.

## 6. Quality and security

- **Reliability:** N/A — the slice adds no writer beyond the send/move verbs,
  no retry, and no queue of its own; delivery is pull (`OB-11`) and a missed
  wake or notice changes nothing held.
- **Accessibility:** N/A — no served markup exists or is added (ADR-053).
- **Privacy:** bodies travel only to readers of the sender board (`OB-10`);
  wakes carry ids only (`OB-13`); notices and headlines never carry bodies
  (`OB-14`, `OB-15`); redaction before emission covers every new envelope and
  notice (`WATCH-10` via `OB-12`).
- **Security:** fail-closed refusals for unknown recipients/kinds/statuses,
  illegal transitions, wrong-actor moves, over-bound bodies, and mismatched
  cursors — every refusal writes nothing (ADR-008). No new capability is
  granted: sending writes only to boards the caller may write; reading gathers
  only boards the caller may read.
- **Security — oracle analysis:** the cross-board gather (`OB-10`) is the
  slice's oracle surface, and it is closed by construction: denied rows are
  skipped, never named, and the skip is indistinguishable from absence in
  output bytes, exit codes, refusal sentences, and counts. The wrong-actor
  move refusal (`message {id} is addressed to {recipient}`, `OB-06`) names the
  recipient only to a caller who already reads the row — it leaks nothing to
  anyone denied. A7's attention-resolve refusal quotes an `m-` id only on a
  path the caller already invoked with that id.
- **Security — secrets analysis:** messages are correspondence, not vaults.
  `OB-17` keeps secrets out by practice (a `SHOULD`, explicitly not an
  enforced gate — no detector is built and none is claimed); the enforced
  halves are structural: bodies never enter wakes (`OB-13`), heartbeats
  (`OB-15`), or notices (`OB-14`); redaction precedes emission everywhere
  envelopes render.
- **Operability:** one queue per identity gathered across readable boards
  (`OB-10`); settled rows archive out of the operational set (ADR-021 via
  `OB-20`); listings refuse over-limit rather than truncate (ADR-037 via
  `OB-20`); pane wakes stay idle-only without George's authority (`OB-19`).
- **Performance:** observations, not budgets — sends and moves are single-row
  inserts plus one chained event, the same shape as existing attention writes;
  the 250ms watch poll (`rust/watch.rs:15`) propagates to `--to` streams
  unchanged; the N-minute filter is caller-supplied display arithmetic, never a
  delivery SLA (see ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | What is the body byte bound enforced by `OB-04`? Proposed: 16 KiB, following the `METADATA_LIMIT` observation (`rust/watch.rs:16`) — a precedent, not a commitment. | George | open | implementation (no send path exists until the bound is set) |
| OQ-2 | What is the exact CLI argv (verb names, flag spellings, `--unanswered` units, default inbox limit)? Proposed: the §5 grammar. | George | open | implementation (the refusal sentences in §3 hold regardless) |
| OQ-3 | Which named registered roles beyond the team lead enter the `OB-02` recipient registry at launch? | George | open | implementation (the registry seeds from George's list) |
| OQ-4 | Does the `watch --to` flag land in `t-fde5d91c` (WATCH row) or in this slice's implementation row? Proposed: `t-fde5d91c`, specified once here. | George | open | implementation scheduling only, not the spec gate |

Closing note, in the manner of `docs/specs/watch.md:375-380`: the board rows
behind this document (`t-bf2458c3`, `e-ea261014`, `t-fde5d91c`) and George's
2026-09-29 decisions and approval were taken from the delegating contract, not
read directly — this worktree has no KB route and no Ord route. Where the
contract and this document differ on a row fact, the contract wins and this
document is corrected. Open questions to George arrive as a card; this document
files none itself.

## 8. Verification

Every test name below is planned, not existing: each is marked `planned`, no
name is claimed to exist at the baseline, and no name was enumerated with
`cargo test -- --list` because there is no message test to enumerate — a
`grep` for a `message` subcommand over `rust/` at the baseline finds only the
unrelated `commit message` prose. Each is a compiled-binary exchange at the
named layer, landing in the `## Requirements trace — docs/specs/outbox.md`
matrix section on implementation. The matrix carries `none`/`none` rows with
`no e2e coverage` until implementation enumerates the real names; this table is
the draft it copies, and the deliberate difference is stated on both sides
rather than hidden.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `OB-01` | `MUST` | `process` | planned: `message_send_addresses_named_recipient_and_mints_m_id` | asserts the id-shape, duplicate-`--id`, unknown-recipient, and missing-addressee refusals; `no e2e coverage` |
| `OB-02` | `MUST` | `process` | planned: `message_recipient_classes_refuse_outsiders_and_yield_to_overlap` | asserts the class refusal and that an overlap refusal stands beside a message; `no e2e coverage` |
| `OB-03` | `MUST` | `process` | planned: `message_kind_restricted_to_four_and_immutable` | asserts the kind refusal sentence; `no e2e coverage` |
| `OB-04` | `MUST` | `process` | planned: `message_body_over_bound_refused_nothing_written` | asserts the `exceeds` sentence at the OQ-1 bound; `no e2e coverage` |
| `OB-05` | `MUST` | `process` | planned: `message_task_link_and_thread_reply_land_in_sender_inbox` | asserts the missing-task refusal and the non-enumerating reply denial; `no e2e coverage` |
| `OB-06` | `MUST` | `process` | planned: `message_lifecycle_moves_one_step_gated_by_role` | asserts both refusal sentences and the wrong-actor refusal; `no e2e coverage` |
| `OB-07` | `MUST` | `process` | planned: `unread_repeats_until_acked_across_all_carriers` | asserts repeat-then-silence on ack; `no e2e coverage` |
| `OB-08` | `MUST` | `process` | planned: `inbox_lists_open_first_headlines_only_with_n_filter` | asserts ordering, no-bodies, and the N filter; `no e2e coverage` |
| `OB-09` | `MUST` | `process` | planned: `message_lives_on_sender_board_only` | asserts no recipient-board copy and the write-authz refusal; `no e2e coverage` |
| `OB-10` | `MUST` | `process` | planned: `cross_board_gather_skips_denied_rows_without_oracle` | asserts byte-identical absence vs nonexistence plus cursor-mismatch fail-closed; `no e2e coverage` |
| `OB-11` | `MUST` | `process` | planned: `pull_delivers_without_push_queue_holds_unsurfaced_rows` | asserts the queue holds what no wake or notice surfaced; `no e2e coverage` |
| `OB-12` | `MUST` | `process` | planned: `watch_to_mirrors_inbox_under_watch_rules` | asserts stream/queue parity, cursor binding, and no second dialect; owed with `t-fde5d91c`; `no e2e coverage` |
| `OB-13` | `MUST` | `process` | planned: `dispatcher_wake_carries_pointer_only_and_respects_reads` | asserts id-only payload and no wake for denied rows; `no e2e coverage` |
| `OB-14` | `MUST` | `process` | planned: `piggyback_mail_field_on_objects_arrays_byte_identical` | asserts the five-plus-count cap, no bodies, and identity rules; `no e2e coverage` |
| `OB-15` | `MUST` | `process` | planned: `heartbeat_frames_carry_compact_unread_notice` | asserts notice-on-idle-stream without polling; `no e2e coverage` |
| `OB-16` | `MUST` | `process` | planned: `message_reply_never_verdict_never_resolves_attention` | asserts the resolve refusal quoting the `m-` id and unchanged attention status; `no e2e coverage` |
| `OB-17` | `SHOULD` | `process` | planned: `message_pointer_names_location_not_content` | practice, not a gate; `no e2e coverage` |
| `OB-18` | `MUST` | `process` | planned: `m_ids_refuse_claims_handoffs_leave_queue_unchanged` | `no e2e coverage` |
| `OB-19` | `MUST` | `process` | planned: `pane_wake_idle_only_without_george_authority` | asserts the queue holds what an unsent wake would carry; `no e2e coverage` |
| `OB-20` | `MUST` | `process` | planned: `settled_messages_archive_and_listings_cap` | asserts ADR-021 archival and the ADR-037 limit refusal; `no e2e coverage` |
| `OB-21` | `MUST` | `process` | planned: `mcp_verbs_match_cli_authz_refusals_oracle` | asserts parity of actor, authz, refusal, and no-oracle rules; `no e2e coverage` |

No `chrome` or `http` evidence is planned: the slice ships no pages and no
served surface at all after the ADR-053 retirement, so process-boundary
exchanges prove what the slice states; the matrix rows say `no e2e coverage`
plainly per the convention at
`docs/testing/compiled-rust-e2e-matrix.md:224-236`.

## 9. Change log

- `2026-09-29` — slice created at `OB-01` .. `OB-21`. No supersessions yet.
