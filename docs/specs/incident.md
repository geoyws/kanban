# Specification: a board tracks incidents as typed rows with a severity, a four-state lifecycle, an append-only timeline, and a reviewed close that refuses a missing review (slice INCIDENT)

## 1. Identity and baseline

- **Slice ID:** `INCIDENT`. Requirement IDs are `INCIDENT-01` .. `INCIDENT-15`, stable across wording
  refinements; numbering is by creation, grouping is by topic (`INCIDENT-15` joined by the
  2026-09-28 OQ-3 verdict).
- **Baseline:** `2026-09-28` at commit `ae2fb8f51ff14844d76317da6739b011a90de391` on branch
  `kanban-geoyws-driver`. Every "today" claim below cites the line that
  has it, as `<absolute-path>:<line>`, read at the baseline commit in
  `/Users/geoyws/work/src/kanban/`. Board schema at the baseline is `33`
  (`/Users/geoyws/work/src/kanban/rust/db.rs:2552`), and no incident surface exists yet: the
  word appears only in prose comments (`/Users/geoyws/work/src/kanban/rust/lib.rs:4812`,
  `/Users/geoyws/work/src/kanban/tests/e2e.rs:41382`), there is no incident route in
  `ROUTE_SHAPES` (`/Users/geoyws/work/src/kanban/rust/serve.rs:631-646`), and no incident
  table, id prefix, or severity enum exists in the model beside `ATTENTION_KINDS`
  (`/Users/geoyws/work/src/kanban/rust/model.rs:933`) and `ATTENTION_STATUSES`
  (`/Users/geoyws/work/src/kanban/rust/model.rs:944`). The specification is written before
  the implementation, as ADR-047 §6 requires (per
  `/Users/geoyws/work/src/kanban/docs/specs/README.md:15-21`).
- **Status:** `SPEC-READY` on 2026-09-28 (re-gate review by IncidentRegate of the owner-dictated
  OQ-3 delta against the SDD §1 exit criteria — `INCIDENT-15` + A12, OQ-3 closed by `a-915fa4cc`,
  no open question remains; writer Spec-incident; findings returned to the main loop for KB
  recording. Product readiness is not claimed by this stamp: it stays `/quality`, then `/tidy`,
  then the served-tier receipt).
- **Owner (product scope):** George. He alone resolves scope, severity meaning, the open questions
  in §7, and whether a non-goal in §2 is reinstated. He approved this slice under epic
  `e-c0852fe7` on 2026-09-28 (the approval ADR-047 §7 requires for a slice outside the `WEB`/`SPA`
  rollout); the board row is task `t-d94b9d1c`.
- **Decider (wording of this document):** Spec-incident, the writer of this slice. Where this
  document and the delegating contract differ on a fact, the contract wins and this document is
  corrected (see the closing note in §7).
- **Sources:**
  - **George, 2026-09-28** (via the delegating contract for task `t-d94b9d1c`): new typed `i-`
    incident; severities `SEV1`–`SEV4`; statuses `open` → `mitigated` → `resolved` → `reviewed`;
    `detected`/`mitigated`/`resolved` timestamps; append-only timeline (`time`/`actor`/`text`);
    links to tasks, attention, deployments, sitreps, and hosts; evidence as a pointer holding no
    secret; no claims on incidents; a reviewed close that refuses an absent written review
    (cause, impact, what worked, what did not) and an absent follow-up task or explicit
    no-follow-up reason; every board; `/incidents` and `/incident/<board>/<id>`; open `SEV1`/`SEV2`
    on Needs you; medic markdown receipts remain evidence; P1 implementation after the makeover
    epic `e-02a9f510`.
  - Source epic `e-b481f61b` on the board is the authority for the decisions above; this slice's
    scope is the docs specification, ADR, and matrix rows only — no product edits.
  - `docs/adr/ADR-052-incidents-are-typed-rows-with-a-reviewed-close.md` — the decision this
    specification implements.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §6 is why this slice is specified before implementation, §9 is
    why §6 carries no invented budget.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why unknown
    severities, unknown statuses, illegal transitions, and unreviewed closes are refused with a
    sentence that names the accepted values or the missing piece, and why a refusal writes
    nothing.
  - `docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md` — what an
    incident is not: attention rows stay decision cards, and this slice links to them rather
    than reusing their resolve ladder (see ADR-052).
  - Shipped surface at the baseline: `BOARD_SCHEMA_VERSION` and `BOARD_MIGRATIONS`
    (`/Users/geoyws/work/src/kanban/rust/db.rs:2552`,
    `/Users/geoyws/work/src/kanban/rust/db.rs:3086-3091`); the served route table
    (`/Users/geoyws/work/src/kanban/rust/serve.rs:631-646`); the Needs-you projection
    (`/Users/geoyws/work/src/kanban/rust/projection.rs:325`); the id-shape refusal precedent
    (`/Users/geoyws/work/src/kanban/rust/model.rs:972`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section `## Requirements trace — docs/specs/incident.md` is the trace
  of record when it lands (convention at
  `/Users/geoyws/work/src/kanban/docs/testing/compiled-rust-e2e-matrix.md:224-236`).

## 2. Purpose and scope

**Intended outcome.** A board records what broke, how bad it is, what happened next, and what was
learned, so an outage is opened once, mitigated and resolved in order, and closed only with a
written review and a follow-up — and never as a task that a lane can claim.

**Users / actors.**

- **Lane agents** — open incidents, append timeline entries, attach links and evidence pointers,
  and move them along the lifecycle from the CLI.
- **The board operator (George)** — authors the written review, decides the follow-up, and reads
  open `SEV1`/`SEV2` incidents on Needs you and on the served pages.
- **Adapter clients (MCP)** — read incidents through the generated surface; no new trust boundary
  is added.

**In scope.** The `i-` incident identity; the severity and status enums; the three timestamps; the
append-only timeline; the five link targets; the evidence pointer; the claim exclusion; the two
reviewed-close refusals; the `/incidents` and `/incident/<board>/<id>` pages; the Needs-you rule
for open `SEV1`/`SEV2`; the board schema migration; process-boundary and served-bytes evidence
for each mandatory requirement.

**Boundaries.**

- **Attention (ADR-042) is linked, not reused.** Incident rows do not resolve through the
  decision-card composer and do not share the attention resolve/reopen ladder; an incident links
  to an attention row when the discussion lives there.
- **Medic markdown receipts stay the evidence format.** This slice adds pointers to them, not a
  replacement for them (`INCIDENT-13`).
- **The CLI grammar is pinned in §3, not guessed later.** The `incident` verbs below follow the
  existing `COMMANDS` conventions (`/Users/geoyws/work/src/kanban/rust/lib.rs:879`): a
  `kanban incident <subcommand>` group, `--as` naming the acting caller on every write,
  `--json` on every verb, `--id i-…` minting per `INCIDENT-01`, and list/show flags shaped
  like `sprint list`/`sprint show`. Former OQ-2 (exact argv) is closed by that pinning; the
  sentences the product says are `INCIDENT-01`/`INCIDENT-02`/`INCIDENT-09`/`INCIDENT-10`
  regardless of argv.
- **Only geoyws marks an incident reviewed (OQ-3, closed 2026-09-28).** George's `a-915fa4cc`
  verdict (choice owner, approve) gates only the `resolved` → `reviewed` transition
  (`INCIDENT-15`): every other move records its acting `--as` caller on the timeline without an
  actor gate, and lane agents author the review text and the follow-up through the existing
  verbs for geoyws's review to carry. Acceptance's reviewed-close proofs run the geoyws actor.

**Non-goals.**

- **A product definition of what each severity means.** `SEV1`–`SEV4` are labels with an order;
  what counts as a `SEV1` is operator practice, not a documented rule (the same cut the
  `COMPLAINT` slice makes for what counts as a complaint).
- **A second ordering on the list page.** `/incidents` contains exactly the set §3 fixes; display
  order is unspecified in this slice and no test pins it.
- **Paging, search indexing, or notification policy for incidents.** Search, watch, and
  notification behaviour for the new rows is untouched; if a later slice touches it, it specifies
  it then (ADR-047 §6).

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that will prove the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

Refusal sentences below are quoted verbatim, with `{value}` and `{id}` as the substitution
points. Every refusal exits non-zero and writes nothing — no row, no timeline entry, no link, no
event, no migration side effect (ADR-008 fail-closed). The CLI argv below is the pinned
contract (former OQ-2, closed in §2); the sentences are what the product says regardless of
the argv that reaches them.

### Identity, severity, and lifecycle

**INCIDENT-01** — Open a typed incident with a severity.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (typed `i-` incident, `SEV1`–`SEV4`).
An incident opens with an id of the form `i-<suffix>` (lowercase `i`, a hyphen, a non-empty
suffix of letters, digits, dot, underscore, or hyphen — the same shape the `sp-` precedent fixes
at `/Users/geoyws/work/src/kanban/rust/model.rs:972`) and a severity of exactly one of
`SEV1`, `SEV2`, `SEV3`, `SEV4`, on every board; no board opts out. A severity outside the four is
refused with `invalid incident severity {value}; expected SEV1, SEV2, SEV3, or SEV4`.
CLI: `kanban incident open TEXT --severity SEV1|SEV2|SEV3|SEV4 --as ACTOR [--id i-…]
[--json]`, following the `sprint new` shape
(`/Users/geoyws/work/src/kanban/rust/lib.rs:239`). Incident rows carry no tags: reads take
board scope, and tag-scoped denials do not apply to a row with no tags. The id is
operator-suppliable
with `--id` and otherwise minted as `i-` plus eight hex characters (the `sp-` minting precedent
at `/Users/geoyws/work/src/kanban/rust/store.rs:8748-8766`). An id of any other shape is refused
with `incident id must start with i-, include a suffix, be at most 64 ASCII characters, and
contain only letters, digits, dot, underscore, or hyphen` (the `sp-` sentence at
`/Users/geoyws/work/src/kanban/rust/model.rs:972` with the prefix substituted); a `--id` that
names an incident that already exists on the board is refused with `incident {id} already
exists`, and writes nothing — no row, no timeline entry, no event.
Data rules: the id and severity are written once at open and never edited afterwards; the row
opens at status `open` with `detected_at` set (`INCIDENT-03`).
Permissions: the opener is recorded as the timeline actor of the open entry. OQ-3 decided only
the reviewed transition (`INCIDENT-15`); no other move carries an actor gate.
*Superseded 2026-09-28.* The permissions sentence formerly read "the operator gate, if any, is
OQ-3"; George's `a-915fa4cc` verdict (choice owner, approve) closed OQ-3 on the reviewed
transition alone: only geoyws effects `resolved` → `reviewed`, while lane agents author the
review text and the follow-up.

**INCIDENT-02** — Move status forward along one chain, one step at a time.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (`open` → `mitigated` →
`resolved` → `reviewed`).
The only status values are `open`, `mitigated`, `resolved`, `reviewed`, and the only moves are
`open` → `mitigated`, `mitigated` → `resolved`, and `resolved` → `reviewed` (the last under the
`INCIDENT-09`/`INCIDENT-10` gates). Any other target value is refused with `invalid incident
status {value}; expected open, mitigated, resolved, or reviewed`; any other move between two
valid values is refused with `incident {id} is {status} and cannot move to {target}`.
CLI: `kanban incident mitigate ID --as ACTOR [--json]`, `kanban incident resolve ID --as ACTOR
[--json]`, and `kanban incident review` for the gated close (`INCIDENT-09`/`INCIDENT-10`) —
distinct verbs per move, following the `sprint start`/`sprint close`/`sprint abandon` precedent
rather than one `--to` flag. Failure behaviour: the refused row keeps its status and
timestamps, and no timeline entry and no event is appended. Skipped steps (e.g.
`open` → `resolved`) are refused by the sentence above — the former skip-step question is
closed on the conservative rule already in this requirement; any looser rule arrives as a
supersession, never as a reinterpretation.

**INCIDENT-03** — Stamp detected, mitigated, and resolved exactly once each.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (three timestamps).
`detected_at` is set at open and defaults to the creation time when the opener names none;
`mitigated_at` is set exactly once, by the `open` → `mitigated` move and by nothing else;
`resolved_at` is set exactly once, by the `mitigated` → `resolved` move and by nothing else.
Data rules: the three stamps are never edited after they are set, and the invariant
`detected_at <= mitigated_at <= resolved_at` always holds once the later stamps exist.

### Timeline, links, and evidence

**INCIDENT-04** — Append timeline entries of time, actor, and text; never edit history.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (append-only timeline).
A timeline entry carries exactly `time`, `actor`, and `text`. Entries are appended only; there is
no edit and no delete, and an append never rewrites an earlier entry's bytes. The open move
(`INCIDENT-01`), each status move (`INCIDENT-02`), each link change (`INCIDENT-05`), and the
review (`INCIDENT-09`) each append their own entry, so the timeline is the complete order of what
happened to the row.
CLI: `kanban incident note ID TEXT --as ACTOR [--json]`, following the `note ID TEXT` shape
(`/Users/geoyws/work/src/kanban/rust/lib.rs:170`). The acting `--as` caller is recorded as the
entry's actor.

**INCIDENT-05** — Link tasks, attention rows, deployments, sitreps, and hosts.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (five link targets).
An incident links to any combination of task ids, attention ids, deployment ids, sitrep ids, and
host names. Task, attention, deployment, and sitrep targets MUST exist on the same board or the
link is refused, quoting the missing target and writing nothing; host links are opaque strings
and are never resolved, dialled, or validated — no DNS, no reachability check, no invented
availability claim. Removing a link appends a timeline entry naming what was removed; the entry
itself is never removed (`INCIDENT-04`).
CLI: `kanban incident link ID (--task TASK | --attention ATT | --deployment DEP | --sitrep SR |
--host NAME) ... --as ACTOR [--json]` and `kanban incident unlink ID (--task TASK |
--attention ATT | --deployment DEP | --sitrep SR | --host NAME) ... --as ACTOR [--json]`.
Authorization is the board's existing tag authorization on both verbs: the caller's readable and
writable tag scope is checked as the containing board checks it, and a caller who may not read
the row receives the existing non-enumerating `denied or not found`
(`/Users/geoyws/work/src/kanban/rust/authz.rs:34`) rather
than a shape that distinguishes unknown from unreadable.

**INCIDENT-06** — Record evidence as a pointer, stored verbatim and never expanded.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (evidence pointer).
The evidence field stores the pointer string the writer gives — a receipt path, a URL, or a
receipt id — byte for byte. The product MUST NOT fetch, inline, render, or otherwise expand the
target: reading the incident back returns the pointer it was given and no fetched content.
A medic markdown receipt path is an accepted pointer value (`INCIDENT-13`).
CLI: `kanban incident evidence ID --pointer TEXT --as ACTOR [--json]`. Setting the pointer
replaces the previous pointer string and appends a timeline entry; it never fetches the target.

**INCIDENT-07** — Keep secrets out of the incident row.
Strength: `SHOULD` · Layer: `process` · Source: George 2026-09-28 (no secret).
The evidence pointer SHOULD name where the evidence lives rather than carrying pasted content,
and secret material SHOULD NOT be pasted into any incident field. This is operator practice, not
an enforced gate: enforcement would need a secret detector, which this slice does not build
(ADR-047 §9 forbids inventing the commitment). A `SHOULD` exception records its reason; it does
not fail the gate.

**INCIDENT-08** — Incidents are never claimed and never hold a lease.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (no claims).
No claim, lease, handoff-accept, or scheduler verb takes an `i-` id: pointing one at an incident
is refused, quoting the id, and leaves the incident and its timeline untouched. Incidents never
appear in claim candidates and never carry a lease token. The claimable queue is exactly what it
was at the baseline for every board.

### The reviewed close

**INCIDENT-09** — Refuse a reviewed close without a written four-part review.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (review: cause, impact, what
worked, what did not).
The `resolved` → `reviewed` move carries a written review with four named parts: cause, impact,
what worked, and what did not. A move missing the review, or missing any one part, is refused
with `incident {id} cannot be marked reviewed without a written review naming cause, impact,
what worked, and what did not`. The accepted review is stored on the row and appended to the
timeline verbatim.
CLI: `kanban incident review ID --as ACTOR --cause TEXT --impact TEXT --worked TEXT
--did-not-work TEXT [--followup t-… | --no-followup-reason TEXT] [--json]`, following the
`attention resolve ID --as ACTOR --choice KEY [--note TEXT]` shape (one resolving verb carrying
its gate material as flags). Who effects the transition is decided: only `--as geoyws` moves a
row to `reviewed` (`INCIDENT-15`).
*Superseded 2026-09-28.* This paragraph formerly left authorship to OQ-3 (board attention
`a-915fa4cc`, still `OPEN`), with the verb recording the acting `--as` caller until George
answered it. George's `a-915fa4cc` verdict (choice owner, approve) closed it: lane agents author
the review text and the follow-up, and geoyws alone marks the incident reviewed.

**INCIDENT-10** — Refuse a reviewed close without a follow-up task or a recorded reason for none.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (follow-up task or explicit
no-follow-up reason).
The `resolved` → `reviewed` move carries either a follow-up task id that exists on the same
board, or an explicit recorded reason for no follow-up. A move with neither is refused with
`incident {id} cannot be marked reviewed without a follow-up task or a recorded reason for
none`; a move naming a task that does not exist is refused, quoting that task id, and writes
nothing. The accepted follow-up (or reason) is stored on the row and appended to the timeline
verbatim. Both `INCIDENT-09` and this requirement MUST hold at once: a close missing both is
refused, and the refusal names the review first. The follow-up half rides the same
`kanban incident review` argv (`--followup` / `--no-followup-reason`); `--followup` naming a
task on another board is refused as a missing target on this board (`INCIDENT-05` same-board
rule), quoting that task id and writing nothing.

**INCIDENT-15** — Only geoyws marks an incident reviewed; lane agents author the review text and
the follow-up.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 via board attention `a-915fa4cc`
(OQ-3 verdict, choice owner, approve).
The `resolved` → `reviewed` transition is effected only when the acting `--as` caller is
`geoyws`, carrying the four-part review (`INCIDENT-09`) and the follow-up task or recorded
reason (`INCIDENT-10`). Any other actor attempting the transition is refused with `incident {id}
cannot be marked reviewed by {actor}; only geoyws marks an incident reviewed`, and nothing is
written — no status change, no stamp, no timeline entry, no event. Lane agents author the review
text (cause, impact, what worked, what did not) and file or propose the follow-up task through
the existing verbs — `incident note` timeline entries and ordinary task creation — and the geoyws
`incident review` invocation carries that material onto the row verbatim. Earlier status moves
(`INCIDENT-02`) record their acting `--as` caller on the timeline without an actor gate: OQ-3
decided only the reviewed transition, and this requirement fixes no gate beyond it.

### Served surface

**INCIDENT-11** — Serve the incident list and the incident detail on every board.
Strength: `MUST` · Layer: `unit` · Source: George 2026-09-28 (`/incidents`,
`/incident/<board>/<id>`).
`GET /incidents` renders the incident list and `GET /incident/<board>/<id>` renders the detail:
identity, severity, status, the three timestamps, the timeline in append order, the links, the
evidence pointer as given, and, once closed, the review and the follow-up. The list contains
exactly the set the lifecycle fixes — no filtering by severity or status beyond what
`INCIDENT-12` fixes for Needs you; display order is a non-goal (§2).
Rendering contract: the pages render inside the existing shell and inherit its width, contrast,
and keyboard behaviour (no new interaction pattern, no browser write verb); the JSON projection
beside them answers the same rows the CLI would show the same principal — the same board and
tag authorization enforced inside the store, not by a filter in the route (the SPA-08 rule at
`/Users/geoyws/work/src/kanban/docs/specs/spa.md:205-216`) — and the SPA mounts both routes on
that projection, reading `bodyHtml`-style typeset content where prose renders and never a second
query language in the web layer. CLI reads stay primary: `kanban incident list [--status
open|mitigated|resolved|reviewed] [--severity SEV1|SEV2|SEV3|SEV4] [--all] [--limit N] [--json]`
and `kanban incident show ID [--json]` answer exactly what the pages render.
Tenancy: naming an unreadable, unknown, or retired board — or an unknown or unreadable `i-` id —
produces the one non-enumerating refusal: the HTML arm answers the existing not-found page and
the JSON arm answers `denied or not found`, with no body that distinguishes unknown from
unreadable and no enumeration oracle in a list (unreadable rows are skipped, never refused
mid-enumeration, per `/Users/geoyws/work/src/kanban/rust/authz.rs:213-218`).
No write verb is added to the browser: the served surface stays read-only under ADR-016, and the
reviewed close stays a CLI/MCP write.

**INCIDENT-12** — Show open SEV1 and SEV2 incidents on Needs you.
Strength: `MUST` · Layer: `unit` · Source: George 2026-09-28 (open SEV1/2 on Needs you).
An incident with status `open` and severity `SEV1` or `SEV2` appears on the Needs-you queue
(`/Users/geoyws/work/src/kanban/rust/projection.rs:325` — the `needs_you` projection, whose
per-board `Store::attention` merge the incident entries join); every other
combination — `mitigated`, `resolved`, `reviewed`, or `SEV3`/`SEV4` at any status — does not.
The existing attention cards on that queue are unchanged: same order, same shape, same counts
apart from the added incident entries.

**INCIDENT-13** — Leave medic markdown receipts working exactly as today.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (medic receipts remain evidence).
Producing, storing, and reading a medic markdown receipt behaves byte-identically with incidents
present: same commands, same paths, same bytes. The only new behaviour is that a receipt path is
an accepted `INCIDENT-06` pointer. A board that never opens an incident is observably unchanged
apart from the schema version bump that `INCIDENT-14` requires.

### Storage and migration

**INCIDENT-14** — Migrate every existing board forward with zero data loss, re-runnably.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-28 (every board).
A new `BOARD_Vnn` migration is appended at the end of `BOARD_MIGRATIONS`
(`/Users/geoyws/work/src/kanban/rust/db.rs:3086-3091`) and `BOARD_SCHEMA_VERSION`
(`/Users/geoyws/work/src/kanban/rust/db.rs:2552`) advances by exactly one. No version number is
pinned here: the baseline is V33, and the next free version is fixed at integration time,
because two sibling slices race for V34 (tasks `t-14` and `t-2cff`); the landing change names
the first free `BOARD_Vnn` after the other lands. Opening an older
board with the new binary migrates it in place: the new incident tables arrive empty, and every
existing row — tasks, attention, deployments, sitreps, sprints, rules, search indexes — survives
with its content, status, and timestamps intact. Data rules: the migration writes the new schema
and `user_version` only; it invents no incident row and edits no existing row. The runner still
refuses a newer-than-supported board rather than writing to it, exactly as at the baseline.

## 4. Acceptance examples

### A1 (`INCIDENT-01`, `INCIDENT-03`, `INCIDENT-04`)

*Given* an empty board at the migrated schema,
*when* a lane opens an incident with severity `SEV2`,
*then* the open prints a row with an id of the form `i-<suffix>`, severity `SEV2`, status `open`,
and `detected_at` set; and the timeline holds exactly one entry — the open — carrying a time, the
opener as actor, and the opening text.

### A2 (`INCIDENT-01`)

*Given* a board at the migrated schema,
*when* a lane opens an incident with severity `SEV5`,
*then* the write is refused with `invalid incident severity SEV5; expected SEV1, SEV2, SEV3, or
SEV4`, and the board holds no new row and no new event. The same sentence (with the offered value
substituted) answers any other value outside the four.

### A3 (`INCIDENT-02`, `INCIDENT-03`)

*Given* an open `SEV1` incident,
*when* a lane moves it to `mitigated` and then to `resolved`,
*then* the first move sets `mitigated_at` and appends a timeline entry, the second sets
`resolved_at` and appends again, and `detected_at <= mitigated_at <= resolved_at` holds;
*when* a lane then tries to move the resolved incident back to `open`,
*then* the move is refused with `incident {id} is resolved and cannot move to open`, and the row,
its stamps, its timeline, and the event tail are all unchanged. The same refusal shape answers a
`resolved` → `reviewed` attempt that skips nothing but lacks its gates (see A7).

### A4 (`INCIDENT-04`)

*Given* a `mitigated` incident with two timeline entries,
*when* a lane appends a text entry as a named actor,
*then* the timeline holds three entries in append order, the first two byte-identical to before,
and the third carries the given time, actor, and text; there is no verb that edits or removes any
of the three.

### A5 (`INCIDENT-05`, `INCIDENT-06`)

*Given* a board holding a task `t-x`, an attention row, a deployment, a sitrep, and a medic
receipt path,
*when* a lane links all five to an open incident and records the receipt path as evidence,
*then* the incident reads back all five links and the evidence pointer byte-for-byte, with no
fetched receipt content anywhere on the row;
*when* a lane links a task id that does not exist,
*then* the link is refused, quoting that id, and the incident's links and timeline are unchanged.
Host links are stored as given without any lookup.

### A6 (`INCIDENT-08`)

*Given* an open incident,
*when* a lane points a claim at its `i-` id,
*then* the claim is refused, quoting the id, the incident keeps its status with no new timeline
entry, and claim candidates never list it. Task claiming around it behaves exactly as at the
baseline.

### A7 (`INCIDENT-09`, `INCIDENT-10`)

*Given* a `resolved` incident,
*when* a lane moves it to `reviewed` with no review and no follow-up,
*then* the move is refused with `incident {id} cannot be marked reviewed without a written review
naming cause, impact, what worked, and what did not`, and nothing is written;
*when* the lane supplies a four-part review but neither a follow-up task nor a reason for none,
*then* the move is refused with `incident {id} cannot be marked reviewed without a follow-up task
or a recorded reason for none`, and nothing is written;
*when* the lane supplies the review, a reason for no follow-up, and moves again,
*then* the incident reads `reviewed`, the review and the reason are stored and appended verbatim,
and a follow-up task id that does not exist is refused quoting that id instead.

### A8 (`INCIDENT-11`)

*Given* a board with one open and one reviewed incident,
*when* a client reads `GET /incidents` and `GET /incident/<board>/<id>` for each,
*then* the list page contains exactly both incidents with their ids, severities, and statuses;
each detail page renders identity, severity, status, the three timestamps, the timeline in append
order, the links, the evidence pointer as given, and — on the reviewed one — the review and the
follow-up; and `GET /incident/<board>/i-nope` answers the existing not-found page.
*When* a caller who may not read the board names either route, or names an `i-` id it may not
read, *then* the HTML arm answers the same not-found page and the JSON arm answers
`denied or not found` — indistinguishable from unknown — and a list never names what was
skipped. `kanban incident list` and `kanban incident show` answer the same rows to the same
principal.

### A9 (`INCIDENT-12`)

*Given* four incidents — open `SEV1`, open `SEV2`, open `SEV4`, and mitigated `SEV1`,
*when* the Needs-you queue is projected,
*then* it contains the open `SEV1` and the open `SEV2` and neither of the other two, and every
attention card on the queue is byte-identical to the queue without incidents.

### A10 (`INCIDENT-13`)

*Given* a medic markdown receipt written before this slice,
*when* a lane produces a new receipt, reads both back, and records the new path as an
`INCIDENT-06` pointer,
*then* both receipts read byte-identical to the baseline format, and the incident row carries the
path and nothing fetched from it.

### A11 (`INCIDENT-14`)

*Given* a board at schema 33 seeded with a task, an attention row, a deployment, and a sitrep,
*when* the new compiled binary opens it and then opens one incident,
*then* the board reports the next free schema version after integration (baseline V33; the exact
`BOARD_Vnn` is fixed at landing, not here — see `INCIDENT-14`); all four seeded rows survive with
content, status, and timestamps intact; and re-opening the migrated board migrates nothing
further and changes nothing.

### A12 (`INCIDENT-15`, `INCIDENT-09`, `INCIDENT-10`)

*Given* a `resolved` incident whose timeline carries agent-authored review text and whose board
holds a filed follow-up task,
*when* a lane actor invokes `kanban incident review` with the four-part review and the follow-up,
*then* the move is refused with `incident {id} cannot be marked reviewed by {actor}; only geoyws
marks an incident reviewed`, and the row stays `resolved` with nothing written — no status
change, no stamp, no timeline entry, no event;
*when* geoyws invokes the same verb with the same material,
*then* the incident reads `reviewed`, with the review and the follow-up stored on the row and
appended to the timeline verbatim.

Where the template asks for more categories: invalid input is A2 (plus the `INCIDENT-01` id-shape
and duplicate-`--id` refusals); unauthorised access is the second half of A8 plus A12 — the
reviewed-transition actor gate is decided (OQ-3 closed by `a-915fa4cc`: only geoyws,
`INCIDENT-15`), and beyond it this specification fixes no permission beyond the existing
board/tag authorization it inherits; failure recovery needs no
scenario beyond A11 because the slice adds no new failure mode
— a migration step that fails leaves `user_version` unmoved, since the version bump commits only
with the step; idempotency is the re-open half of A11; concurrency needs no scenario because the
slice adds no write path beyond the existing single-writer migration transaction.

## 5. Contracts and data

- **Interface version or schema:** the CLI gains the pinned `incident` group — `open`, `mitigate`,
  `resolve`, `review`, `note`, `link`/`unlink`, `evidence`, `list`, `show` — and the
  `schema --json` surface publishes them with no renamed operation and no changed flag elsewhere;
  the served pages gain `GET /incidents` and `GET /incident/<board>/<id>` beside the baseline
  route table, with a JSON projection answering the same rows to the same principal and no new
  write verb in the browser.
- **Data invariants:** incident ids are unique per board and match `i-<suffix>`; severity is always
  one of four values; status is always one of four values and moves only forward along
  `INCIDENT-02`; `detected_at <= mitigated_at <= resolved_at` once the later stamps exist;
  timeline entries are append-only triples of `time`/`actor`/`text`; the evidence field holds a
  pointer string, never expanded content; no incident row carries a lease or claim column.
- **Migration:** forward only, from V33 to the next free `BOARD_Vnn` after integration,
  appended at the end of `BOARD_MIGRATIONS`. No backfill:
  existing boards gain empty incident storage, and no incident row exists except by a fresh open.
- **Compatibility:** a baseline (V33) binary opening a newer board stops at the runner's guard and
  writes nothing. A newer binary opening a V33 board migrates it forward on first touch.
- **Ownership:** the board owns its incident rows, timeline entries, and links; the operator
  (geoyws) owns the review and the follow-up decision on each one and alone effects the reviewed
  transition — lane agents author the review text and file or propose the follow-up (`INCIDENT-15`,
  OQ-3 closed). Medic receipts stay owned by whatever produces them today; this slice moves no
  ownership.

## 6. Quality and security

- **Reliability:** N/A — the slice adds no new failure mode: opens, moves, links, and the two
  reviewed-close gates refuse fail-closed with no partial write, and the migration runs inside
  the runner's per-step transaction.
- **Accessibility:** the two new pages render inside the existing shell and inherit its width,
  contrast, and keyboard behaviour; no new interaction pattern is introduced (no new browser
  write verb).
- **Privacy:** incident rows inherit the containing board's tenancy and tag visibility
  (`INCIDENT-05`, `INCIDENT-11`): the timeline records the acting caller's existing identity and
  nothing else, and no new retention rule is invented.
- **Security — ASVS applicability:** OWASP ASVS **5.0.0** is selected as verification guidance,
  not a certification claim; individual requirement IDs are **not** claimed (the 5.0.0 text was
  not reviewed for this slice — the `docs/specs/web-ui.md` §6 cut). By chapter theme: access
  control applies because incident reads and writes inherit the existing board/tag authorization
  enforced inside the store (`INCIDENT-05`, `INCIDENT-11`; non-enumerating `denied or not found`
  per `/Users/geoyws/work/src/kanban/rust/authz.rs:34`); validation and encoding applies because
  severity, status, id shape, and the review/follow-up gates refuse fail-closed with sentences
  that name the fix (`INCIDENT-01`, `INCIDENT-02`, `INCIDENT-09`, `INCIDENT-10`); stored content
  is agent-authored text rendered inert by the existing shell, with the evidence pointer never
  fetched or expanded (`INCIDENT-06`). Authentication, session management, and cryptography add
  no incident-specific contract: this slice inherits the existing trusted-edge identity and
  creates no secret, credential, or session mechanism. `INCIDENT-07` records the
  keep-secrets-out practice as a `SHOULD`, explicitly not an enforced gate.
- **Operability:** a failed migration step leaves the board at its prior `user_version` with
  prior data intact, because the version bump commits only with the step; the operator re-runs
  any ordinary command to retry, and a board already at the landed version migrates nothing.
- **Performance:** observation only, not a budget — steady-state incident writes are single-row
  inserts plus one timeline entry, the same shape as existing attention writes; no timing
  commitment is made (see ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | May a status move skip a step (e.g. `open` → `resolved`)? | George | closed — decided conservative: no; `INCIDENT-02` as written is the whole rule and tests pin it; a looser rule arrives as a supersession, not a reinterpretation | none — P1 implementation follows `INCIDENT-02` |
| OQ-2 | What is the exact CLI verb and flag grammar for the create, status, timeline, link, evidence, and review writes? | George | closed — pinned in `INCIDENT-01`/`INCIDENT-02`/`INCIDENT-04`/`INCIDENT-05`/`INCIDENT-06`/`INCIDENT-09`/`INCIDENT-10` and §5 | none — P1 implementation follows the pinned argv |
| OQ-3 | Who may move an incident and author its review — the operator only, or any writer? | George | closed 2026-09-28 — verdict `a-915fa4cc` (choice owner, approve): only geoyws marks an incident reviewed; lane agents author the review text and file or propose the follow-up (`INCIDENT-15`, proven by A12) | none — P1 implementation follows `INCIDENT-15` |

OQ-3 was the tracked form of the §2 boundary note. George closed it (closing `a-915fa4cc`) on the
reviewed transition alone: `INCIDENT-15` carries the permission clause, acceptance's reviewed-close
proofs run the geoyws actor, and no open question remains.

Note on sources: the board rows behind this document (task `t-d94b9d1c`, epics `e-c0852fe7` and
`e-02a9f510`, source epic `e-b481f61b`) and George's 2026-09-28 approval and decisions were taken
from the delegating lane contract, not read directly — this worktree's constraints allow no KB
access. An independent reviewer with board access confirms the quotations against those rows
before any gate.

## 8. Verification

Every test name below is planned, not existing: each is marked `planned`, no name is claimed to
exist at the baseline, and no name was enumerated with `cargo test -- --list` because there is no
incident test to enumerate — a `grep` for `incident` over `rust/` and `tests/` at the baseline
finds only prose comments. Each is a compiled-binary exchange at the named layer, landing in the
`## Requirements trace — docs/specs/incident.md` matrix section on implementation. The matrix
carries `none`/`none` rows with `no e2e coverage` until implementation enumerates the real names;
this table is the draft it copies, and the deliberate difference is stated on both sides rather
than hidden.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `INCIDENT-01` | `MUST` | `process` | planned: `incident_open_assigns_typed_id_and_severity` | asserts the severity, id-shape, and duplicate-`--id` refusals; `no e2e coverage` |
| `INCIDENT-02` | `MUST` | `process` | planned: `incident_status_moves_forward_one_step_only` | asserts both refusal sentences and the skipped-step refusal; `no e2e coverage` |
| `INCIDENT-03` | `MUST` | `process` | planned: `incident_timestamps_stamp_once_and_order` | `no e2e coverage` |
| `INCIDENT-04` | `MUST` | `process` | planned: `incident_timeline_appends_without_edit_or_delete` | exercises `incident note`; `no e2e coverage` |
| `INCIDENT-05` | `MUST` | `process` | planned: `incident_links_tasks_attention_deployments_sitreps_hosts` | includes the missing-target refusal and the non-enumerating denial; `no e2e coverage` |
| `INCIDENT-06` | `MUST` | `process` | planned: `incident_evidence_pointer_stored_verbatim_never_expanded` | exercises `incident evidence`; `no e2e coverage` |
| `INCIDENT-07` | `SHOULD` | `process` | planned: `incident_evidence_pointer_names_location_not_content` | practice, not a gate; `no e2e coverage` |
| `INCIDENT-08` | `MUST` | `process` | planned: `incident_ids_refuse_claims_and_leave_queue_unchanged` | `no e2e coverage` |
| `INCIDENT-09` | `MUST` | `process` | planned: `incident_reviewed_close_refuses_missing_four_part_review` | asserts the A7 sentence via `incident review`; `no e2e coverage` |
| `INCIDENT-10` | `MUST` | `process` | planned: `incident_reviewed_close_needs_followup_or_reason` | asserts the A7 sentences including the cross-board follow-up refusal; `no e2e coverage` |
| `INCIDENT-11` | `MUST` | `unit` | planned: `incident_pages_render_list_and_detail_bytes` | reads the served bytes of both routes plus the CLI list/show parity and the A8 denial half; `no e2e coverage` |
| `INCIDENT-12` | `MUST` | `unit` | planned: `needs_you_carries_open_sev1_sev2_only` | `no e2e coverage` |
| `INCIDENT-13` | `MUST` | `process` | planned: `medic_receipts_unchanged_beside_incidents` | `no e2e coverage` |
| `INCIDENT-14` | `MUST` | `process` | planned: `incident_migration_carries_prior_board_forward_empty` | seeds a V33 board, migrates to the next free version, re-opens; `no e2e coverage` |
| `INCIDENT-15` | `MUST` | `process` | planned: `incident_reviewed_transition_is_geoyws_only` | asserts the A12 refusal sentence for a lane actor and the geoyws close carrying agent-authored material; `no e2e coverage` |

No `chrome` evidence is planned: the slice adds two read-only pages with no new interaction
pattern and no browser write verb, so the served-bytes `unit` evidence plus process-boundary
exchanges prove what the slice states; the matrix rows say `no e2e coverage` plainly per the
convention at
`/Users/geoyws/work/src/kanban/docs/testing/compiled-rust-e2e-matrix.md:224-236`.

## 9. Change log

- `2026-09-28` — slice created at `INCIDENT-01` .. `INCIDENT-14`. First supersessions arrived the
  same day (see the OQ-3 entry below).
- `2026-09-28` — independent `/quality` spec pass (wording only, no readiness stamp): pinned the
  `incident` CLI argv (closed OQ-2), closed the skip-step question on the conservative one-step
  rule (OQ-1), added id minting/invalid/duplicate refusals, same-board tag authz with
  non-enumerating refusals, the JSON/API/SPA rendering contract with ASVS 5.0.0 applicability,
  and the next-free-version migration rule; canonicalized source citations to
  `/Users/geoyws/work/src/kanban/`; OQ-3 (`a-915fa4cc`) stays `OPEN` so the slice stays `BLOCKED`.
- `2026-09-28` — OQ-3 closed by George's `a-915fa4cc` verdict (choice owner, approve): added
  `INCIDENT-15` (only geoyws marks an incident reviewed; lane agents author the review text and
  file or propose the follow-up) with acceptance A12 and a verification row; superseded the OQ-3
  `OPEN` sentences under `INCIDENT-01`/`INCIDENT-09` and updated the §2/§4/§5/§7 notes. Slice IDs
  now `INCIDENT-01` .. `INCIDENT-15`; no other requirement changed.
