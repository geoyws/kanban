# Specification: watch steers by lane and note kind, and its envelope carries lane, type and priority the same way on kb and Ord (slice WATCH)

## 1. Identity and baseline

- **Slice ID:** `WATCH`. Requirement IDs are `WATCH-01` .. `WATCH-12`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-28` at commit `edb07459d4bf1e36ffbb18cf621a7ddc15cfd1a7` (detached at
  `origin/kanban-geoyws-driver`) in `/Users/geoyws/work/wt/kanban-t-28dca81e-watch-41a533`. Every
  "today" claim below cites the line that has it, as `<path>:<line>`, read in that worktree.
  Board schema at the baseline is `34` (`rust/db.rs:2563`, `BOARD_SCHEMA_VERSION`), and neither
  `watch --lane`, nor `watch --note-kind`, nor the envelope `lane`/`type`/`priority`/
  `priorityLevel` keys exist yet: the specification is written before the implementation, as
  ADR-047 §6 requires (per `docs/specs/README.md:15-22`).
- **Status:** `DRAFT` — gate requested 2026-09-28. (SPEC-READY is stamped here by an independent
  reviewer against the SDD reference's §1 exit criteria, not by the writer of this document. It
  authorises neither implementation nor rollout nor release.)
- **Owner (product scope):** George. He alone resolves scope, whether a non-goal in §2 is
  reinstated, and the open questions in §7.
- **Decider (wording of this document):** slice row `t-28dca81e` under epic `e-c0852fe7`, approved
  on owner verdict `a-e240ed2d` (choice `watch`, outcome `approve`) — the approval ADR-047 §7
  requires for a slice outside the `WEB`/`SPA` rollout. (Read directly from the kb board:
  attention `a-e240ed2d`, kind `approval`, choice `watch` — Approve WATCH slice and promote draft —
  resolved by geoyws 2026-09-28: `WATCH slice approved; t-28dca81e promoted; t-fde5d91c proceeds once docs/specs/watch.md is SPEC-READY`.)
- **Sources:**
  - Slice row `t-28dca81e` — the scope under approval: `watch --lane` cursor binding, note-kind
    steer, envelope `lane`/`type`/`priority`, and shared kb/Ord conformance against Ord
    `t-49703f52` as a read-only reference (conformance described, Ord untouched).
  - Owner verdict `a-e240ed2d` (choice `watch`, `approve`) — the WATCH slice approved under
    ADR-047 §7.
  - Implementing row `t-fde5d91c` — the gated product code. It stays gated until this
    specification lands, so §8 names `none` where the new surface has no test yet rather than
    inventing one.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §6 is why this slice is specified before implementation, §7 is why
    the row above counts as approval, §9 is why §6 carries no invented budget.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why an unknown
    note kind, a mismatched cursor, and a board-semantic predicate on registry scope are refused
    with a sentence that names the accepted values, and why a refusal writes nothing.
  - Shipped surface at the baseline: `rust/watch.rs:34-48` (`StreamKey`, the bound predicate
    set), `rust/watch.rs:50-66` (`ScopeEnvelope`), `rust/watch.rs:66-90` (`CursorToken`,
    `deny_unknown_fields`, every newer field `#[serde(default)]`), `rust/watch.rs:156-157`
    (`--follow` requires `--limit` of at least 1), `rust/watch.rs:493-549` (the `advanced`
    heartbeat — the only place a cursor moves without an event), `rust/watch.rs:577-648`
    (`project_event`: the additive protocol-v1 projection — `board`, `eventID`, `timestamp`,
    `subject`, `relations`, `priorStatus`, `currentStatus`, `tags`, bounded `metadata` — with
    `_semanticV1` hash-covered but never emitted and secrets redacted recursively),
    `rust/watch.rs:672-730` (normalization: statuses, relations, the bound key), and
    `rust/watch.rs:15-16` (`POLL_INTERVAL` 250ms, `METADATA_LIMIT` 16 KiB — observations, not
    budgets).
  - The values the new predicates range over: `NOTE_KINDS` with exactly six values
    (`rust/model.rs:441-443`: `plan`, `progress`, `blocker`, `decision`, `evidence`, `done`),
    selected today on `note` (`rust/lib.rs:1710-1715`); the task row's `lane`
    (`rust/model.rs:492`), `type` (`rust/model.rs:486-487`), `priority` (`rust/model.rs:497`)
    and its operator-facing projection `priority_level` (`rust/model.rs:472-479`: 0-2 `P0`,
    3-5 `P1`, 6-9 `P2`); the stored event row (`rust/model.rs:713-724`).
  - The existing evidence this slice preserves: nine `watch_*` cases plus one `the_watch_*` case in `tests/e2e.rs` plus
    `revoking_authority_stops_a_live_watch_stream_without_a_reconnect` in
    `tests/authz_bypass_matrix_e2e.rs` — every name enumerated with
    `cargo test --locked --test e2e -- --list` and
    `cargo test --locked --test authz_bypass_matrix_e2e -- --list` on 2026-09-28 in this
    worktree — and the coverage note at `docs/testing/compiled-rust-e2e-matrix.md:630-638`.
  - Ord `t-49703f52` (read-only reference, unread from this worktree — no `acies` CLI and no
    `ACIES_*` environment here) via the `/ord` skill's `.result` envelope convention; the row-field
    shapes (`task list`/`task show` rows carrying the task's lane, type and priority,
    `attention list` rows carrying the card's kind and lane) are assumptions for A6 to confirm,
    not skill-documented shapes. The Ord column of the §5 table states expectations to be
    verified against that row at implementation (OQ-1); the kb column states what the code does
    today.
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section is the trace of record and the two are kept identical by the same
  change.

## 2. Purpose and scope

**Intended outcome.** One long-running `watch` stream answers "what changed for my lane, of the
note kinds I care about, in an envelope one consumer reads against kb and Ord alike" — without
a second trace, a second cursor dialect, or a flag that silently means nothing.

**Users / actors.**

- Agent lanes, which hold one `watch --follow` stream per lane and steer it with `--lane`,
  `--note-kind` and the existing predicates rather than filtering a firehose client-side.
- The shared kb/Ord consumer (dispatcher adapters, queue bridges), which reads the envelope's
  `lane`/`type`/`priority` with identical names, casing, shapes and null rules on both boards.
- George, who approves the slice (done, `a-e240ed2d`) and resolves the open question in §7.

**In scope.** The `watch` CLI grammar (`--lane`, `--note-kind`); their normalization and cursor
binding; the four additive envelope keys on board scope and their explicit nulls on registry
scope and legacy events; the kb-vs-Ord conformance table; the refusals the new predicates add.

**Boundaries.** The event ledger and its hashes (untouched — new keys are projected, never
stored); the dispatcher and subscription rows (they keep their own `(subscriptionID,eventID)`
identity; §2 of that surface is not re-specified here); the Ord board itself (read-only
reference — OQ-1 — no Ord row is written by this slice); the browser and the served pages (no
served markup changes, so no browser evidence exists for any requirement below).

**Non-goals.**

- Cross-board fan-in: exactly one scope stays active at a time, as today. A consumer watching
  two boards holds two streams.
- A new cursor version: `CursorToken.version` does not move; the new fields ride
  `#[serde(default)]` like `kinds`/`relations`/`prior_statuses`/`current_statuses` before them,
  so cursors minted before this slice still parse.
- Any performance, availability, latency or retention commitment: the 250ms poll and the 16 KiB
  metadata bound are cited observations (§6), never budgets.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:
`process` — a compiled-binary process-boundary exchange, no HTTP and no browser (this slice
changes no served markup, so there is no browser surface to drive).

### Lane binding and steering

**WATCH-01** — `watch --lane` steers by subject lane.
Strength: `MUST` · Layer: `process` · Source: slice row `t-28dca81e`.
`Repeatable --lane LANE restricts delivery to events whose subject task row carries one of the
named lanes. Values within the lane family are ORed; the family is ANDed with every other
predicate family (--kind, --note-kind, --relation, --prior-status, --current-status, --tag).
Matching is literal against the task row's lane; a lane that matches nothing yields an empty
stream, not an error.`
`Permissions: the caller's board/tag read scopes gate the stream — revoking authority stops a live stream without a reconnect (revoking_authority_stops_a_live_watch_stream_without_a_reconnect, tests/authz_bypass_matrix_e2e.rs:1041); watch itself grants nothing and writes nothing.`
`Failure behaviour: none beyond WATCH-04 on registry scope.`
`Data rules: the predicate selects from stored rows; it writes nothing, archives nothing, and
survives no restart beyond the persisted opaque cursor that carries it (WATCH-02).`

**WATCH-02** — The lane set binds to the cursor.
Strength: `MUST` · Layer: `process` · Source: the shipped cursor-binding rule
(`rust/watch.rs:34-48`, `rust/watch.rs:66-90`, `rust/watch.rs:716-730`).
`The normalized (sorted, deduplicated) lane set joins the bound predicate set in the StreamKey,
the ScopeEnvelope and the CursorToken. Reusing a persisted cursor with any different normalized
lane set fails closed before any row is read, exactly like a changed kind/relation/status/tag
set today.`
`Failure behaviour: the reuse is refused with a sentence naming the mismatch; nothing is
emitted on stdout and no cursor advances.`

**WATCH-03** — The scope envelope echoes the bound lanes.
Strength: `MUST` · Layer: `process` · Source: slice row `t-28dca81e` (typo-safety without a lane
registry).
`Every emitted NDJSON envelope carries the call's normalized lane set in its scope object, so a
caller that misspells a lane sees an empty stream beside the exact set it asked for rather than
an indistinguishable silence. Lanes are free-form — no master file exists to fail closed
against — so literal match plus echo is the whole contract.`

**WATCH-04** — Registry scope refuses lane steering.
Strength: `MUST` · Layer: `process` · Source: the shipped registry-scope rule (registry scope
supports `--kind` only and rejects board semantic predicates).
`--lane on --registry (or --rule) scope fails closed before any row is read, with a sentence
naming the rejected flag. No partial stream precedes the refusal.`

### Note-kind steer

**WATCH-05** — `watch --note-kind` steers note delivery by note kind.
Strength: `MUST` · Layer: `process` · Source: slice row `t-28dca81e`; `NOTE_KINDS`
(`rust/model.rs:441-443`).
`Repeatable --note-kind KIND restricts note_added events to notes whose kind is in the named
set: plan, progress, blocker, decision, evidence, done. Values within the family are ORed; the
family is ANDed with every other family, so under a note-kind predicate an event with no note
kind does not pass. The normalized set binds to the cursor exactly like WATCH-02.`
`Failure behaviour: an unknown note kind fails closed with the ADR-008 sentence that names all
six accepted values; the refusal writes nothing. A note-kind predicate on registry scope fails
closed like WATCH-04.`

### Envelope lane, type and priority

**WATCH-06** — Board-scope envelopes carry the subject's lane, type and priority.
Strength: `MUST` · Layer: `process` · Source: slice row `t-28dca81e`; task row
(`rust/model.rs:486-499`).
`The board-scope projection adds four top-level keys beside the existing v1 keys: lane (the
subject task row's lane), type (its type: epic, story or task), priority (its 0-9 queue key)
and priorityLevel (its P0/P1/P2 projection via priority_level). All four are additive — every
v1 key keeps its name, casing, shape and position. Where there is no subject task (registry
scope, taskless events) or the event predates subject semantics (legacy events), each of the
four is explicit null, never absent.`
`Data rules: the projection reads only the task row's type, lane, priority and derived level;
it never reads a lease token, a credential, or any secret-bearing column, and it passes through
the same recursive redaction as the rest of the envelope (WATCH-10).`

### Shared kb/Ord conformance

**WATCH-07** — One envelope dialect on kb and Ord.
Strength: `MUST` · Layer: `process` · Source: slice row `t-28dca81e` against Ord `t-49703f52`
(read-only; OQ-1).
`The §5 conformance table is normative: for every row it names, the kb envelope key, the Ord
envelope key, the casing, the JSON shape and the null-vs-absent rule are identical, so one
consumer parses both boards without a per-board branch. Any deviation found at implementation
against Ord t-49703f52 is a failure of this requirement, not a dialect to document — the table
is corrected or the projection is, in the same change.`

### Preserved mechanism

**WATCH-08** — Cursors from before this slice still parse.
Strength: `MUST` · Layer: `process` · Source: `CursorToken` `#[serde(default)]` convention
(`rust/watch.rs:66-90`).
`The new lane and note-kind cursor fields default when absent, so a cursor persisted by an older
binary resumes under the new binary with those families empty (unfiltered). Unknown fields are
still denied: a cursor minted by a newer binary fails closed on an older one rather than
silently dropping its lane set.`

**WATCH-09** — The v1 envelope stays byte-stable apart from the four keys.
Strength: `MUST` · Layer: `process` · Source: `project_event` (`rust/watch.rs:577-648`).
`No existing projected key is renamed, recased, reshaped, moved or dropped; _semanticV1 stays
hash-covered and never emitted; the metadata bound (16 KiB, truncated with bytes beside it)
still applies to the event payload as today and does not cover the four new top-level keys. A
consumer pinned to the pre-slice field set reads the new envelopes without a change.`

**WATCH-10** — New keys pass through the existing redaction.
Strength: `MUST` · Layer: `process` · Source: the shipped redact-before-emission rule
(`rust/watch.rs:577-648`).
`Lane names, types and priorities are projected after redaction exactly like every other
payload field; a value that matches a secret shape is redacted rather than emitted, and
redaction still happens before the metadata bound is measured.`

**WATCH-11** — The advanced heartbeat covers the new predicates.
Strength: `MUST` · Layer: `process` · Source: `needs_advanced_heartbeat`
(`rust/watch.rs:493-549`).
`When the lane/note-kind predicates skip a committed unmatched tail, an advanced heartbeat moves
the opaque cursor to the last scanned row, carrying the call's full normalized predicate set
including lanes and note kinds. Idle heartbeats still do not advance the durable cursor. A
--follow stream over sparse matches never re-loops the same rows.`

**WATCH-12** — Follow and limit rules apply unchanged to steered streams.
Strength: `MUST` · Layer: `process` · Source: `rust/watch.rs:156-157`; the shared
`0..1000000` ceiling.
`--follow still requires --limit of at least 1 (`--follow --limit 0` fails with the at-least-1
sentence); sparse filtering by lane and note kind still happens before --limit slices, so the
limit bounds the filtered result set, never the raw scan.`

## 4. Acceptance examples

Concurrency needs no example: watch writes nothing, two streams share no mutable state, and
one scope per stream (§2) keeps two consumers independent. Unauthorized access is read-authority gating rather
than a write refusal: per-poll authority re-read stops a revoked live stream without a reconnect (revoking_authority_stops_a_live_watch_stream_without_a_reconnect, tests/authz_bypass_matrix_e2e.rs:1041).

### A1 (WATCH-01, WATCH-02)

*Given* a board with two tasks in lanes `driver-2` and `driver-3`, each holding one note, and a
`watch --task` cursor minted at literal `0` with `--lane driver-2`,
*when* the consumer replays with that cursor and then replays the same cursor string with
`--lane driver-3`,
*then* the first replay emits only the `driver-2` task's events, and the second replay is
refused with a mismatch sentence before any row is read.

### A2 (WATCH-03, WATCH-01)

*Given* a `watch --lane driver-2 --lane driver-9` stream over a board where no task has lane
`driver-9`,
*when* the consumer reads the NDJSON envelopes to end of backlog,
*then* every envelope's scope object carries the normalized set `["driver-2","driver-9"]`, and
every event payload belongs to a `driver-2` subject — the empty half is visible as a set the
caller can check, not as silence.

### A3 (WATCH-04, WATCH-12)

*Given* no board selector and `--registry --lane driver-2`,
*when* the consumer starts the stream,
*then* the command fails closed naming `--lane` before any row is read; and separately,
*given* `--follow --limit 0` with any lane set,
*when* the consumer starts the stream,
*then* it fails with the at-least-1 sentence.

### A4 (WATCH-05)

*Given* a task holding one `blocker` note and one `progress` note,
*when* the consumer replays `watch --task <id> --cursor 0 --note-kind blocker`,
*then* only the `blocker` note's `note_added` event is emitted; and
*when* the consumer passes `--note-kind urgent`,
*then* the command fails closed with a sentence naming all six accepted kinds and writes
nothing.

### A5 (WATCH-06, WATCH-09, WATCH-10)

*Given* a `P1` task in lane `driver-2` with one note, and a registry-scope stream,
*when* the consumer replays the board scope and the registry scope,
*then* the board envelope carries `lane: "driver-2"`, `type: "task"`, `priority: 4`,
`priorityLevel: "P1"` beside unchanged v1 keys (`board`, `eventID`, `timestamp`, `subject`,
`relations`, `priorStatus`, `currentStatus`, `tags`, `metadata`), while the registry envelope
carries all four as explicit `null`; and no envelope at either scope contains a lease token or
a `leaseToken` key.

### A6 (WATCH-07)

*Given* the §5 table and read access to Ord `t-49703f52`,
*when* the implementer places one kb envelope and one Ord envelope for the same logical event
(a P1 task in a named lane receiving a decision note) side by side,
*then* every table row matches field-for-field — same key, same casing, same shape, same
null-vs-absent rule — or the mismatch is filed as a failure of WATCH-07 with the table and the
projection corrected in the same change.

### A7 (WATCH-08, WATCH-11)

*Given* a cursor persisted by the pre-slice binary and a board whose newest fifty events match
no lane in `--lane driver-2`,
*when* the consumer resumes with the old cursor plus the lane predicate under `--follow`,
*then* the old cursor is accepted with the lane family empty-until-specified, delivery skips
the fifty unmatched rows, and exactly one `advanced` heartbeat advances the opaque cursor to
the scanned tail — the stream never re-loops those fifty rows.

## 5. Contracts and data

- **Interface version or schema:** the `watch` CLI grammar gains two repeatable flags,
  `--lane LANE` and `--note-kind plan|progress|blocker|decision|evidence|done`, on board and
  task scope; registry/rule scope rejects both (WATCH-04, WATCH-05). The NDJSON envelope stays
  protocol version 1 with four additive top-level keys (`lane`, `type`, `priority`,
  `priorityLevel`); `CursorToken.version` does not move and the two new cursor fields ride
  `#[serde(default)]` (WATCH-08). Errors and diagnostics stay on stderr; envelopes on stdout
  only.
- **Data invariants:** watch writes no rows at any scope; predicates select from stored task,
  note and event rows; the opaque cursor remains bound to source, selector, normalized predicate
  set, archive state and last consumed seq. `priorityLevel` is derived at projection time via
  `priority_level` (`rust/model.rs:472-479`), never stored.
- **Migration:** none — no table changes, no schema version move. Cursors persist across the
  cutover by default (WATCH-08).
- **Compatibility:** pre-slice consumers read new envelopes unchanged (WATCH-09); pre-slice
  cursors resume (WATCH-08); newer cursors fail closed on older binaries instead of dropping
  predicates.
- **Ownership:** the board owns tasks, notes and events; the registry owns rules; Ord owns its
  own rows — this slice writes to neither board and describes conformance only (OQ-1).

### Conformance table (normative for WATCH-07)

`kb key` is the NDJSON envelope key this slice specifies; `Ord key` is the catalogue envelope
key expected on Ord; `Shape` is the JSON shape both carry; `Absent rule` is what the key holds
when there is no value. The kb column is observed in `rust/watch.rs:577-648` (existing rows)
and §3 above (new rows); the Ord column states the expectation against `t-49703f52` to be
verified at implementation (OQ-1, A6). Only the `.result` envelope convention is grounded in
the `/ord` skill; the row-field shapes (task rows carrying lane, type, priority; attention
rows carrying kind and lane) are assumptions A6 must confirm.

| # | Field | kb key | Ord key | Shape | Absent rule |
| --- | --- | --- | --- | --- | --- |
| 1 | lane | `lane` | `lane` | string, the subject task's lane (`driver-2`) | explicit `null`, never absent |
| 2 | type | `type` | `type` | string, one of `epic`, `story`, `task` (`TASK_TYPES`, `rust/model.rs:440`) | explicit `null`, never absent |
| 3 | priority | `priority` | `priority` | integer 0-9 queue key | explicit `null`, never absent |
| 4 | priority level | `priorityLevel` | `priorityLevel` | string `P0`/`P1`/`P2` via the published 0-2/3-5/6-9 mapping | explicit `null`, never absent |
| 5 | subject | `subject` | `subject` | object `{"type":"task","id":"<id>"}` | explicit `null` (registry/taskless/legacy) |
| 6 | relations | `relations` | `relations` | array of `{"kind":"parent\|ancestor\|depends-on","type":"<t>","id":"<id>"}` | `[]` when the subject task simply has none; `null` where there is no subject task (registry/taskless/legacy) |
| 7 | prior status | `priorStatus` | `priorStatus` | string task status | explicit `null` |
| 8 | current status | `currentStatus` | `currentStatus` | string task status | explicit `null` |
| 9 | tags | `tags` | `tags` | sorted string array of registered tags | `[]` when none |
| 10 | kind | `kind` | `kind` | string event/note kind from the board's published enum | always present on events; note-kind steer values are the six `NOTE_KINDS` on both sides |
| 11 | cursor | `cursor` (envelope) | `cursor` (envelope) | opaque string, bound to the normalized predicate set | never absent on backlog/follow frames; idle heartbeats do not advance it |

## 6. Quality and security

- **Reliability:** N/A — the slice adds no writer, no retry and no queue; delivery stays
  at-least-once per subscription identity on the dispatcher surface, which this slice does not
  re-specify.
- **Accessibility:** N/A — no served markup changes.
- **Privacy:** redaction before emission is preserved and extended to the new keys (WATCH-10);
  the scope echo (WATCH-03) carries only the caller's own predicate values.
- **Security:** fail-closed refusals for unknown note kinds, registry-scope lane/note-kind use,
  mismatched cursors and malformed/future cursors (WATCH-02, WATCH-04, WATCH-05); refusals write
  nothing (ADR-008). No new capability is granted: watch stays read-only.
- **Security — ASVS applicability:** OWASP ASVS **5.0.0** is selected as verification guidance,
  not a certification claim. **V5.1** applies to fail-closed validation of the new predicates
  (WATCH-02, WATCH-04, WATCH-05); **V8.3.1** applies because the board/tag read scopes gating
  each stream are enforced by the trusted Store per poll, not by stream parameters (WATCH-03,
  §4); **V14.2.6** applies because envelopes disclose only the minimum data — secrets redacted
  recursively (WATCH-10) and the scope echo carrying only the caller's own predicates (WATCH-03).
  Authentication, session-management and cryptography chapters add no WATCH-specific contract
  because this slice inherits the existing identity, creates no secret, credential or session mechanism.
- **Operability:** one scope per stream (non-goal, §2); sparse filtering before `--limit`
  (WATCH-12); the `advanced` heartbeat keeps sparse follow streams moving (WATCH-11).
- **Performance:** observations, not budgets — the 250ms poll (`rust/watch.rs:15`), the 16 KiB
  metadata bound (`rust/watch.rs:16`), and the shared `0..1000000` limit ceiling propagate to
  steered streams unchanged; no new number is introduced.

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Does every Ord-column expectation in the §5 table hold verbatim against Ord `t-49703f52` (keys, casing, shapes, null rules)? | `t-fde5d91c` implementer | open | implementation (not the spec gate: the kb column is observed in code, the acceptance bar is field-for-field in A6, and the mismatch rule in WATCH-07 makes any deviation a build failure rather than an unpriced surprise) |

Closing note, in the manner of `docs/specs/complaint.md:17-21`: the board rows behind this
document (`t-28dca81e`, `e-c0852fe7`, `a-e240ed2d`, Ord `t-49703f52`) were taken from the
delegating contract, not read directly — this worktree has no KB route (no `hosts.tsv`, so
`kb-board` refuses) and no Ord route (no `acies` CLI, no `ACIES_*` environment). Where the
contract and this document differ on a row fact, the contract wins and this document is
corrected.

## 8. Verification

Planned evidence for every mandatory requirement. The matrix section
(`## Requirements trace — docs/specs/watch.md WATCH-01..WATCH-12`) is the trace of record;
this table is its draft and the two land identical. `none` says `no e2e coverage` plainly and
names the row that must write it: the gated implementation `t-fde5d91c`. Every other name was
enumerated with `cargo test --locked --test e2e -- --list` and
`cargo test --locked --test authz_bypass_matrix_e2e -- --list` on 2026-09-28 in this worktree.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `WATCH-01` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: `--lane` does not exist at the baseline. |
| `WATCH-02` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: cursor carries no lane set at the baseline. |
| `WATCH-03` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`. |
| `WATCH-04` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`. |
| `WATCH-05` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: no note-kind predicate at the baseline. |
| `WATCH-06` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: the four keys are not projected at the baseline. |
| `WATCH-07` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c` with OQ-1 readback: A6's side-by-side is the evidence. |
| `WATCH-08` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: no new cursor fields exist yet to default. |
| `WATCH-09` | MUST | process | `the_watch_surface_matches_help_and_the_mcp_manifest_excludes_it`, `watch_emits_truthful_bounded_semantic_envelopes` | additive stability on the shipped surface; `t-fde5d91c` re-runs both against envelopes carrying the four new keys. |
| `WATCH-10` | MUST | process | `watch_emits_truthful_bounded_semantic_envelopes` | the redaction half of that case; re-run with lane/type/priority-bearing events. |
| `WATCH-11` | MUST | process | `watch_follow_delivers_an_event_queued_behind_interleaved_heartbeats` | the heartbeat half; extended to lane/note-kind-skipped tails by `t-fde5d91c`. |
| `WATCH-12` | MUST | process | `watch_drains_backlogs_in_bounded_batches_and_rejects_invalid_limits`, `watch_follow_still_refuses_a_zero_limit` | limit-before/after-filtering and the at-least-1 refusal; re-run under steered predicates. |

Preserved-behaviour witnesses (not mapped 1:1 above, kept green by the same run):
`watch_replays_resumes_and_respects_selector_boundaries`,
`watch_follow_streams_new_events_and_keeps_outputs_separated`,
`watch_filters_sparse_history_and_binds_normalized_predicates_to_cursors`,
`watch_replays_removed_subjects_and_keeps_registry_semantics_separate`,
`watch_rejects_malformed_unsupported_and_future_cursors`,
`revoking_authority_stops_a_live_watch_stream_without_a_reconnect`.

## 9. Change log

- `2026-09-28` — slice created at `WATCH-01` .. `WATCH-12`. No supersessions yet.
