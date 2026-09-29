# Specification: a done move needs a foreign-actor review verdict (slice DG)

## 1. Identity and baseline

- **Slice ID:** `DG`. Requirement IDs are `DG-01` .. `DG-18`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-29` at commit `f8e1719` on branch `wt/t-7038c70a-spec`. Every
  "today" claim below cites the line that has it, as `<path>:<line>`.
- **Status:** `SPEC-READY` on 2026-09-29 (independent reviewer applying the SDD §1 exit criteria
  over three rounds: one material finding (DG-16 misstated the epic direct-move baseline) and
  four stale line citations, all fixed in-tree and re-verified against the cited lines.
  Specification readiness only — it authorises neither implementation, nor rollout, nor release.)
- **Owner (product scope):** George. He alone resolves scope. On 2026-09-29
  (attention `a-741f1212`) he settled the five open mechanism questions: the verdict
  home (new append-only table), the write grammar (CLI verb), the SHA meaning and
  staleness rule, enablement (per-board flag, off by default, acies first), and
  task-only scope. The answers are pinned as requirements in §3; §7 records them
  as answered.
- **Decider (wording of this document):** the `t-7038c70a` writer.
- **Sources:**
  - kb kanban `t-7038c70a`, DRAFT row signed by `@:geoyws/acies/driver` planner at
    George's request 2026-09-29: "`task move ID done` refused unless the row carries
    a review-verdict record {reviewer actor, reviewed SHA(s), verdict pass,
    evidence}; reviewer MUST differ from claim holder / closing actor; verdict
    points at the attention decisions checked; `--force` needs `--as geoyws` and is
    recorded as an override event; verdict written by the planner/main loop that
    spawned the reviewer, never by the executor."
  - George's design principle (same row): "the reviewer works from kb itself — row,
    linked decisions, pinned diff — decoupled from executor prompts; the ledger
    makes that path the only way to reach done."
  - George's slice approval (attention `a-741f1212`, 2026-09-29), deciding the
    §7 questions as pinned in §3: OQ-1, a new append-only `verdicts` table
    (schema migration) — pinned in DG-11; OQ-2, a CLI verb (e.g.
    `task verdict add ID ... --as PLANNER`), refused when the writer or the
    reviewer is the claim holder or the actor moving the task to done — pinned in
    DG-12; OQ-3, reviewed SHA = full 40-char commit SHA(s) published on origin, a
    verdict stale once the task records a newer head after the verdict, a stale
    verdict never opening the gate — pinned in DG-13 and DG-14; OQ-4, a per-board
    audited flag, off by default, acies first — pinned in DG-15; OQ-5, per task
    only, stories and epics projecting done from children with no extra verdict —
    pinned in DG-16. Only `geoyws` may force past the gate, recorded as an
    override event — pinned in DG-05 and DG-06.
  - Incident: acies `t-5a50c576` closed as implemented at `4700c04` with no
    reviewer; 7 defects found later (acies `t-c24e5027`).
  - Shipped surface at the baseline: `rust/store.rs:5379`-`rust/store.rs:5465`
    (`move_task`: actor, `--force` seizes a live lease via `require_free_lease`,
    `task_moved` event), `rust/store.rs:5417`-`rust/store.rs:5427`
    (`is_work_bearing_status` gate site; `--force` deliberately not consulted for
    prerequisites), `rust/store.rs:2814`-`rust/store.rs:2838` (lease refusal names
    the holder and the `--force` way out), `rust/model.rs:846` (`OPERATOR_ACTOR`
    is `geoyws`), `rust/store.rs:7186`-`rust/store.rs:7195` (attention resolve
    actor rule: only `geoyws` or the raiser may resolve — the `--as geoyws`
    precedent), `rust/store.rs:7366`-`rust/store.rs:7371` (reopen actor rule, same
    shape), `rust/db.rs:3134`-`rust/db.rs:3140` (board schema ladder currently at
    `BOARD_V35`).
  - Gate-site citations re-read in this worktree at `6088ec9` (the baseline's
  branch moved on): `move_task` at `rust/store.rs:5821`, the work-bearing gate
  at `rust/store.rs:5859`, the lease seize at `rust/store.rs:5870`, the
  `task_moved` event at `rust/store.rs:5896`, the story-projection refusal at
  `rust/store.rs:5842`-`rust/store.rs:5852`; `OPERATOR_ACTOR` still
    `rust/model.rs:846`; the attention resolve refusal still
    `rust/store.rs:7746`; the reopen rule still `rust/store.rs:7930`-`rust/store.rs:7935`;
    the schema ladder tip observed here is `BOARD_V36` (`rust/db.rs:2309`, ladder
    `rust/db.rs:3483`-`rust/db.rs:3484`), so §5 names the migration step above
    that tip.
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 names the rows
  this slice will add when it is implemented; this specification does not restate the
  matrix.

## 2. Purpose and scope

**Intended outcome.** A task row reaches `done` only through a reviewer who did
not do the work: with the gate on, the ledger refuses a done-move that carries no
foreign-actor pass verdict, so the incident — a row closed as implemented with no
reviewer and 7 defects found later — cannot recur by the same path.

**Users / actors.**

- Executor lane (claim holder): does the work, holds the lease, moves the row
  toward done. It can never satisfy the gate by its own authorship: no verdict it wrote or reviewed opens its row.
- Reviewer lane: spawned by the planner/main loop, works from kb itself (row,
  linked attention decisions, pinned diff), earns the verdict. It never holds
  the work lease it reviews.
- Planner / main loop: spawns executor and reviewer, writes the verdict the
  reviewer earned to the ledger with `task verdict add ... --as <planner>`. The
  only writer of verdict records.
- George (`geoyws`): the only actor who may override the gate past a refusal, and
  whose override is audited.

**In scope.** Reaching `done` under the gate: the append-only `verdicts`
table (DG-11), the verdict-write verb (DG-12), the SHA shape, publication and
staleness rules (DG-13, DG-14), the foreign-actor rule (DG-03, DG-07), the seven
named refusals (DG-08), the `geoyws`-only `--force` override and its audit event
(DG-05, DG-06), the per-board flag (DG-15), the task-only boundary with the
story/epic projection (DG-16), the every-write cover (DG-17: `task move`,
`checkpoint --state done`, `task add --status done`, import), and the
verdict-list evidence visibility (DG-18).

**Boundaries.**

- The reviewer's working method (what it reads, how it diffs) is owned by the
  lanes that run it, not by this slice; this slice pins only what the ledger
  demands and records.
- Attention resolve/reopen actor rules are owned by their existing specifications;
  this slice cites them as precedent and as the evidence the verdict points at.
- The story-status projection (`rust/store.rs:5842`-`rust/store.rs:5852` in this
  worktree) is owned by the story slice; DG-16 pins the interaction: stories and
  epics project `done` from children and take no verdict.

**Non-goals.**

- **No retroactive verdicts.** Shipped rows that reached `done` without a verdict
  acquire none; the gate governs moves made while it is on, not history.
- **No review quality metric.** The slice pins that a foreign actor checked named
  decisions at a named SHA, not that the check was good; judging reviews is
  George's, not the ledger's.
- **No fail verdicts.** The write verb records a `pass` only (DG-12); a review
  that fails records nothing and the gate stays closed. There is no fail row to
  query and no fail event to audit.
- **No invented performance or availability commitment.** §6 records none; only
  George may turn a measurement into a budget.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

IDs are assigned in creation order and never reused; the groups below are topical.

### The gate

**DG-01** — refuse a gated done-move that carries no pass verdict.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 1.
With the gate on (DG-15), `task move ID done` on a row with no review-verdict
record satisfying DG-02, DG-03, DG-04, DG-13 and DG-14 is refused. The refusal
carries the named reason DG-08 states and leaves the row, its lease and the event
history unchanged.
*Permissions:* applies to every actor including the claim holder and including
`geoyws` without `--force`; DG-06 is the only way past it.

**DG-02** — require the verdict record's five fields.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 1.
A verdict record carries exactly: writer actor, reviewer actor, reviewed SHA(s),
verdict `pass`, and evidence. The writer is the actor from `--as` at write time
and is immutable. A record missing any of the five, with an empty writer, an
empty reviewer, an empty SHA list, or a verdict other than `pass`, does not
satisfy DG-01. The reviewed SHAs are full 40-character commit SHAs published on
origin (DG-13).
*Data rules:* the record is durable ledger state and survives a restart; it is
never silently dropped by a later move, retag or metadata patch.

**DG-03** — refuse a verdict whose reviewer is the worker.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 1.
`claim holder of record` is any actor named as holder in the task's live
`task_claims` row or its claim-event history (`task_claimed`,
`claim_heartbeat`, `claim_released`, `claim_expired` in the `events` table) —
holds or held, including a holder who has since released. The reviewer actor
MUST differ from the claim holder of record for the reviewed
work and from the closing actor attempting the done-move. A verdict naming the
holder or the closer as reviewer is refused as if no verdict existed (DG-01),
with the refusal naming the collision (DG-08 sentence 3). A verdict whose stored
writer is the closing actor is likewise refused as if no verdict existed
(DG-01), with DG-08 sentence 3. The write-time half of this rule is DG-12.
*Failure behaviour:* the row stays where it was; no event records the attempt
beyond the existing denial audit, if any.

### Verified decision as data

**DG-04** — point the verdict at the attention decisions checked.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 2.
The verdict's evidence names the attention decision records the reviewer checked
(their IDs and resolving resolutions), so "verified decision" is ledger data, not
prose. A `pass` verdict citing no decision record does not satisfy DG-01.
*Data rules:* the cited decision records MUST exist and be in `resolved` state at
the done-move; citing an open, missing or reopened row fails the gate.

### Override

**DG-05** — require `--as geoyws` on a `--force` past the gate.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 3; precedent
`rust/store.rs:7746` (attention resolve refusal).
`task move ID done --force` past an unsatisfied gate is refused unless the acting
actor is exactly `geoyws` (`rust/model.rs:846`). Any other actor with `--force`
receives the refusal (DG-08 sentence 4) and the row is unchanged.

**DG-06** — record every gate override as an audited event.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 3.
A `--force` done-move that passes DG-05 writes an override event carrying the
actor (`geoyws`), the task ID, the prior status, and the fact that the gate was
unsatisfied (missing verdict, self-review, or stale SHA — whichever refused it).
The event is hash-chained with the board's other events and survives migration.
*Data rules:* the override marker is never backfilled onto non-override moves and
is never removed by later moves.

### Authorship

**DG-07** — accept verdicts only from the planner loop, never the executor.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 4.
The verdict is written by the planner/main loop that spawned the reviewer; a
verdict write from the executor that did the work (the claim holder, or any actor
holding the work lease at write time) is refused. The ledger distinguishes the
two by actor and lease-hold, not by self-asserted role strings.
*Failure behaviour:* an executor-written verdict is refused with the named reason
(DG-08 sentence 3); no verdict record is stored, no head advances (DG-14), and
DG-01 still refuses the done-move.

### The verdict store (OQ-1)

**DG-11** — keep verdicts in a new append-only `verdicts` table.
Strength: MUST · Layer: process · Source: George 2026-09-29 (attention
`a-741f1212`, OQ-1).
Verdicts live in a new append-only `verdicts` table, one row per stored verdict,
each row carrying the writer actor from `--as` at write time alongside reviewer,
SHAs, verdict and evidence; rows are never updated and never deleted, and a
newer verdict is a newer row. The table lands in the next `BOARD_V` ladder step above
the tip (`BOARD_V37` above the `BOARD_V36` tip observed in this worktree,
`rust/db.rs:2309`, ladder `rust/db.rs:3483`-`rust/db.rs:3484`), migrating forward
only. Boards written before the step open as "no verdict" (DG-01 refuses until
one is earned, never backfilled), and override events (DG-06) survive the
migration.
*Data rules:* a refused write stores nothing; a stored verdict survives restart,
retag, metadata patch and migration.

### The write verb (OQ-2)

**DG-12** — write verdicts only through `task verdict add`, never as the worker.
Strength: MUST · Layer: process · Source: George 2026-09-29 (attention
`a-741f1212`, OQ-2).
The grammar is `task verdict add ID --reviewer ACTOR --sha SHA [--sha ...]
--evidence ATTENTION-ID [--evidence ...] --as WRITER`. It records a `pass`
verdict and nothing else: there is no fail verdict, and a review that fails
records nothing. The write is refused with DG-08 sentence 3 when the writer or
the reviewer is the claim holder of record for the task or holds its work lease
at write time; the closing-actor half of the collision is enforced at move time
by DG-03, since the closer is not yet known at write time. `--as` is mandatory.
*Failure behaviour:* a refused write stores no verdict row and advances no head
(DG-14); DG-01 still refuses the done-move.
*2026-09-29 — the read half of the verdict store is DG-18: `task verdict list` omits evidence ids the caller cannot read.*

### SHAs and staleness (OQ-3)

**DG-13** — cite only full published commit SHAs.
Strength: MUST · Layer: process · Source: George 2026-09-29 (attention
`a-741f1212`, OQ-3).
Each reviewed SHA is a full 40-character lowercase-hex commit SHA. A verdict
citing a short, malformed or empty SHA is refused with DG-08 sentence 5. A
verdict citing a SHA the ledger cannot confirm as published on origin — unknown
or unpublished, wherever the check runs — is refused with DG-08 sentence 6. A
host that cannot confirm publication refuses every citing write with DG-08
sentence 6, without retry or queue.
*Failure behaviour:* a refused write stores nothing (DG-11); a stored verdict
whose SHAs fail these rules does not satisfy DG-01.

**DG-14** — a stale verdict does not open the gate.
Strength: MUST · Layer: process · Source: George 2026-09-29 (attention
`a-741f1212`, OQ-3).
The task's current head is the `head_sha` of its newest work-provenance row for
that task — across `checkpoints`, `task_claims`, `handoffs` and `sitreps`.
Newest is by row time — `checkpoints.created_at`, `task_claims.heartbeat_at`,
`handoffs.created_at`, `sitreps.created_at` — skipping rows with a null head;
ties break toward `task_claims`, then `checkpoints`, then `handoffs`, then
`sitreps`, then the greatest row id/seq. A heartbeat that updates the task's
`task_claims` row counts as re-recorded at its new `heartbeat_at`: a changed
head stales every prior verdict, an unchanged head stales nothing. A verdict
satisfies DG-01 iff its cited SHAs include that current head, compared by exact
string match: a provenance row carrying an abbreviated `--head` (accepted down
to 7 hex characters by `looks_like_head_sha`, `rust/lib.rs:4101`) never matches
a 40-character verdict SHA. `fresh` means citing the current head whenever the
verdict was recorded: a verdict recorded earlier whose SHAs again include the
head after later work satisfies again — staleness is a relation between the
cited SHAs and the current head, not a permanent mark on the verdict. A task
with no non-null head has no current head: no verdict satisfies DG-01 and only
the DG-06 override opens the gate. A stale verdict is refused with DG-08
sentence 2 as if no verdict existed.
*Data rules:* a refused write never moves the head.

### Enablement (OQ-4)

**DG-15** — gate each board with an audited flag, off by default, acies first.
Strength: MUST · Layer: process · Source: George 2026-09-29 (attention
`a-741f1212`, OQ-4).
The gate is per board, stored under the `board_meta` key `done_gate` with value
`on` or `off`; an absent key reads as `off`, and the migration (DG-11) leaves
every existing board absent — off by default. The grammar is
`task verdict gate on|off --as ACTOR`. Every change writes one hash-chained
audit event naming the actor, the board, and the old and new values, with an
absent key audited as old value `off`. The acies board is the first board with
the flag on; this slice turns on no other board.
*Permissions:* only `geoyws` may turn the gate on or off; a toggle from any
other actor is refused with DG-08 sentence 7 and changes nothing.
*Rationale:* a conservative choice matching the DG-05 override precedent;
widening the toggle set needs George.
*Failure behaviour:* with the flag off or absent, DG-09 governs: no verdict is
demanded and no gate refusal is reachable.

### Task-only scope (OQ-5)

**DG-16** — gate tasks only; stories and epics project done from children.
Strength: MUST · Layer: process · Source: George 2026-09-29 (attention
`a-741f1212`, OQ-5).
The gate consults verdict state only for task rows. Story and epic rows reach
`done` through the story projection owned by the story slice: when their gate
completes from done children, no verdict is demanded and none is checked. A
direct `task move` of a story row to `done` stays refused by the projection rule
exactly as at the baseline (`rust/store.rs:5842`-`rust/store.rs:5852` in this
worktree) — that refusal is not a gate refusal and carries no DG-08 sentence. A
direct `task move` of an epic row behaves exactly as at the baseline: an epic's
status is moved by the gate exactly once and then by `task move`
(`rust/store.rs:380`-`rust/store.rs:381`, remedy `rust/store.rs:388`-`rust/store.rs:389`),
so the move succeeds with no verdict demanded and none checked.

### Refusal wording and ungated boards

**DG-08** — refuse in the product's own sentence.
Strength: MUST · Layer: process · Source: ADR-008 (refusal wording is product
contract); `rust/store.rs:2831`-`rust/store.rs:2838` (lease-refusal precedent).
Each gate refusal is one line in fixed wording, with only the bracketed values
varying. The seven sentences, verbatim:

1. No pass verdict: `task {id} has no foreign-actor pass verdict — record one with `task verdict add {id} --reviewer <actor> --sha <sha> --evidence <a-id> --as <planner>` before moving it to done`
2. Stale verdict: `task {id} verdict is stale: it covered {old} but the row now stands at {head} — record a fresh verdict at the new head with `task verdict add {id} --as <planner>``
3. Self-review (writer or reviewer is the claim holder or the closing actor): `task {id} verdict is self-review: {actor} is the claim holder or the closing actor and may be neither the writer nor the reviewer — have the planner loop record a foreign-actor verdict`
4. Non-`geoyws` force: `task {id} done-move past the gate needs `--force --as geoyws` — only geoyws may override, and the override is recorded`
5. Malformed or short SHA: `task {id} verdict refused: `{sha}` is not a full 40-character commit SHA — cite the published commit the reviewer checked`
6. Unknown or unpublished SHA: `task {id} verdict refused: `{sha}` is not published on origin — the verdict must cite a commit SHA published on origin`
7. Non-`geoyws` gate toggle: `done_gate on board {board} unchanged — only geoyws may turn the gate on or off; run `task verdict gate on|off --as geoyws```

Each sentence says what is wrong and the fix. Until an independent reviewer
stamps this slice, no implementation may hard-code them.

**DG-09** — leave ungated boards exactly unchanged.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` acceptance.
With the gate off (DG-15: flag absent or `off`), `task move ID done` behaves
byte-for-byte as at the baseline (`rust/store.rs:5379`-`rust/store.rs:5465`): no
verdict is demanded, no new refusal is reachable, and boards written before the
gate carry no verdict fields and need none.

**DG-10** — gate only the move to done.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask (done-gate, not a
work-gate).
The gate fires only on moves to status `done`. Moves to `in_progress`, `review`,
`blocked`, `todo`, `backlog`, `draft` or `cancelled` never consult verdict state,
and the prerequisite gate (`rust/store.rs:5417`-`rust/store.rs:5427`) is
otherwise unchanged. Stories and epics are DG-16, not an exception to this
requirement.
*2026-09-29 — path coverage extended by DG-17: this requirement keeps the status half (only transitions to `done` consult verdict state); which writes count as reaching `done` is enumerated there.*

### Every write that reaches done (adversarial cover)

**DG-17** — gate every write that makes a task done, not just the move.
Strength: MUST · Layer: process · Source: adversarial review 2026-09-29 of the SPEC-READY slice (George's principle: the ledger makes the verdict path the only way a task reaches done).
With the gate on (DG-15), every write whose resulting status is `done` on a task-type row consults verdict state exactly as `task move ID done` does (DG-01..DG-04, DG-13, DG-14), carrying the DG-08 sentence the move would carry; a refused write lands nothing — no row, no status flip, no `completed_at`, no event beyond the existing denial audit, if any:
- `checkpoint --state done`: the same `enforce_done_gate`, the checkpoint author as the closing actor, no `--force` on this verb. The check runs after the checkpoint row is inserted — the verdict must cover the head this checkpoint records — but before the status flip, inside the one transaction, so a refusal rolls the checkpoint row back too.
- `task add --status done`: a task-type add at `done` is refused with DG-08 sentence 1 (a new row has no holder and no head, and `task add` carries no override grammar).
- import (`import atmux-json|atmux-sqlite`): a task-type row at `done` — inserted or flipped by the upsert — is refused with DG-08 sentence 1 naming the row id, and the whole import rolls back. Import takes any `--as` actor (it is not `geoyws`-only by design), so it fails closed.
Stories and epics stay DG-16: the story-projection advance takes no verdict, and neither does a story- or epic-type row reaching `done` by any of the paths above. `transact`/`batch` items run the same dispatch as the bare commands, so the same store methods — and the same gate — answer them (import and `search-rebuild` are not transactable and keep their own paths, import gated above). No new refusal wording: every refusal on these paths is a DG-08 sentence, so DG-08 is byte-identical.

**DG-18** — hide evidence the caller cannot read from the verdict list.
Strength: MUST · Layer: process · Source: adversarial review 2026-09-29 (attention rows are tag-scoped; see `attention_tags`).
`task verdict list ID` answers the stored verdict rows oldest-first. Under managed enforcement, an evidence id whose attention row the caller cannot read is omitted from its evidence array, so the list never enumerates hidden attention ids; the verdict rows themselves stay — the task read that authorized them is unchanged — and on an unenforcing board the rows answer verbatim. Fail closed: a missing row, denied tags, or a lookup error omits the id. Move-time citation checks (DG-04) still judge every cited id regardless of who is asking.

## 4. Acceptance examples

### A1 (DG-01, DG-08, DG-15)

*Given* the gate on, task `t-x` claimed and worked by executor `lane-e`, with no
verdict record,
*when* `lane-e` runs `task move t-x done --as lane-e`,
*then* the move is refused with `task t-x has no foreign-actor pass verdict — record one with `task verdict add t-x --reviewer <actor> --sha <sha> --evidence <a-id> --as <planner>` before moving it to done`, and the row status,
lease and event count are unchanged.

### A2 (DG-02, DG-04, DG-11, DG-12)

*Given* the gate on, `t-x` worked by `lane-e`, reviewer `lane-r` spawned by the
planner, attention rows `a-1` and `a-2` resolved against `t-x`,
*when* the planner runs `task verdict add t-x --reviewer lane-r --sha <40-char head> --evidence a-1 --evidence a-2 --as planner` and any actor then runs `task move t-x done`,
*then* the write stores one `verdicts` row, the move succeeds, the row is `done`
with `completed_at` set, and the verdict record is still attached to the row.

### A3 (DG-03, DG-08 — self-review refused)

*Given* the gate on and a verdict on `t-x` naming reviewer `lane-e`, who holds or
held the work claim,
*when* `lane-e` runs `task move t-x done --as lane-e`,
*then* the move is refused with `task t-x verdict is self-review: lane-e is the claim holder or the closing actor and may be neither the writer nor the reviewer — have the planner loop record a foreign-actor verdict` and the row is unchanged.

### A4 (DG-05, DG-06, DG-08 — geoyws-only audited override)

*Given* the gate on and `t-x` with no verdict,
*when* `geoyws` runs `task move t-x done --as geoyws --force`,
*then* the move succeeds and one override event exists naming `geoyws`, `t-x`,
the prior status and the unsatisfied-gate reason; *when* `lane-e` runs the same
with `--as lane-e --force`, *then* it is refused with `task t-x done-move past the gate needs `--force --as geoyws` — only geoyws may override, and the override is recorded` and the row is unchanged.

### A5 (DG-07, DG-12, DG-08 — executor cannot write the verdict)

*Given* the gate on and `t-x` claimed by `lane-e`,
*when* `lane-e` runs `task verdict add t-x --reviewer lane-r --sha <40-char head> --evidence a-1 --as lane-e`,
*then* the write is refused with `task t-x verdict is self-review: lane-e is the claim holder or the closing actor and may be neither the writer nor the reviewer — have the planner loop record a foreign-actor verdict`, no
verdict is stored, no head advances, and a later `task move t-x done --as lane-e` is still refused.

### A6 (DG-09, DG-10 — ungated boards and non-done moves unchanged)

*Given* the gate off and `t-y` with no verdict,
*when* its holder runs `task move t-y done` and separately `task move t-y review`,
*then* both succeed exactly as at the baseline with no new refusal reachable and
no verdict field required.

### A7 (DG-02, DG-08 — incomplete verdict)

*Given* the gate on and a planner-written verdict row on `t-x` with an empty
evidence list,
*when* any actor runs `task move t-x done`,
*then* the move is refused with `task t-x has no foreign-actor pass verdict — record one with `task verdict add t-x --reviewer <actor> --sha <sha> --evidence <a-id> --as <planner>` before moving it to done`, and the row is unchanged.

### A8 (DG-13, DG-08 — SHA shape and publication)

*Given* the gate on and `t-x` worked by `lane-e`,
*when* the planner runs `task verdict add t-x --reviewer lane-r --sha abc123 --evidence a-1 --as planner`,
*then* the write is refused with `task t-x verdict refused: `abc123` is not a full 40-character commit SHA — cite the published commit the reviewer checked` and nothing is stored; *when* the planner cites a well-formed but unpublished SHA,
*then* the write is refused with `task t-x verdict refused: `{sha}` is not published on origin — the verdict must cite a commit SHA published on origin` and nothing is stored.

### A9 (DG-14, DG-08 — stale verdict)

*Given* the gate on, `t-x` with a stored verdict covering head `H1`, and a later
checkpoint for `t-x` recording head `H2`,
*when* any actor runs `task move t-x done` relying on the `H1` verdict,
*then* the move is refused with `task t-x verdict is stale: it covered H1 but the row now stands at H2 — record a fresh verdict at the new head with `task verdict add t-x --as <planner>``; *when* the planner records a `pass` verdict covering `H2` citing still-resolved decisions,
*then* the done-move succeeds.

### A10 (DG-15, DG-09 — flag off by default, audited changes)

*Given* a fresh board whose `board_meta` carries no `done_gate` key, and `t-y`
with no verdict,
*when* its holder runs `task move t-y done`,
*then* it succeeds exactly as at the baseline; *when* `geoyws` runs
`task verdict gate on --as geoyws` and the holder retries the done-move,
*then* the move is refused with the DG-08 sentence 1 wording, and one audit
event names `geoyws`, the board, and the `off`-to-`on` change; *when* `lane-e`
runs `task verdict gate off --as lane-e`, *then* it is refused with the DG-08
sentence 7 wording and the flag is unchanged; *when*
`geoyws` runs `task verdict gate off --as geoyws`,
*then* done-moves behave as at the baseline again.

### A13 (DG-02, DG-03, DG-08 — writer may not close)

*Given* the gate on, `t-x` worked by `lane-e`, and a planner-written verdict row
on `t-x` naming foreign reviewer `lane-r` and writer `planner`,
*when* `planner` runs `task move t-x done --as planner`,
*then* the move is refused with `task t-x verdict is self-review: planner is the claim holder or the closing actor and may be neither the writer nor the reviewer — have the planner loop record a foreign-actor verdict` and the row is unchanged; *when* `lane-e` runs `task move t-x done --as lane-e`,
*then* the move succeeds.

### A11 (DG-16, DG-10 — story and epic projection)

*Given* the gate on, story `s-1` with done children and no verdict anywhere, and
parent epic `e-1`,
*when* the story gate completes and the epic flips to `done` through the story
projection,
*then* both succeed with no verdict demanded and none checked; *when* any actor
runs `task move s-1 done --as lane-e` directly,
*then* it is refused by the story-projection rule exactly as at the baseline,
carrying no DG-08 sentence; *when* any actor runs `task move e-1 done --as lane-e`
directly, *then* it succeeds exactly as at the baseline with no verdict demanded
and none checked.

### A12 (DG-11, DG-06 — migration keeps verdicts and overrides)

*Given* a pre-gate board with a `done` row that earned no verdict, migrated
forward to the `verdicts` step,
*when* the board reopens,
*then* the old row carries no verdict and gains none, a new done-move under the
gate is refused until a verdict is earned, and stored verdict rows and override
events are still present with their payloads intact.

### A14 (DG-14 — heartbeat at an unchanged head stales nothing)

*Given* the gate on, `t-x` with a stored verdict covering head `H1`, which is
still the task's current head,
*when* the holder heartbeats the claim without new work so the `task_claims`
row is re-recorded at a newer `heartbeat_at` with the head still `H1`,
*then* the verdict still satisfies DG-01 and a done-move relying on it
succeeds; *when* a later heartbeat (or checkpoint, handoff or sitrep) records
head `H2`,
*then* the same verdict is stale and the done-move is refused with the DG-08
sentence 2 wording until a verdict citing `H2` is recorded — and a still older
verdict whose SHAs also include `H2` satisfies again without a new write.

### A15 (DG-14, DG-06 — a task with no head opens only by override)

*Given* the gate on and `t-x` with no non-null `head_sha` on any of its
`checkpoints`, `task_claims`, `handoffs` or `sitreps` rows,
*when* any actor runs `task move t-x done` citing any stored verdict,
*then* the move is refused and only `geoyws` with `--force` opens the gate,
writing the DG-06 override event.

### A16 (DG-03, DG-08 — a released holder is still the holder of record)

*Given* the gate on, `t-x` claimed and then released by `lane-e` (a
`claim_released` event in its history and no live `task_claims` row), and a
verdict on `t-x` naming reviewer `lane-e`,
*when* any actor runs `task move t-x done`,
*then* the move is refused with `task t-x verdict is self-review: lane-e is the claim holder or the closing actor and may be neither the writer nor the reviewer — have the planner loop record a foreign-actor verdict` and the row is unchanged.

### A17 (DG-17, DG-08 — every write that makes a task done is gated)

*Given* the gate on, task `t-x` claimed and worked by executor `lane-e`, with no
verdict record,
*when* `lane-e` runs `checkpoint t-x --state done`,
*then* the checkpoint is refused with `task t-x has no foreign-actor pass verdict — record one with `task verdict add t-x --reviewer <actor> --sha <sha> --evidence <a-id> --as <planner>` before moving it to done`, and the row status, lease, checkpoints and event history are unchanged; *when* the planner records a foreign-actor `pass` verdict covering the checkpoint's head and `lane-e` retries the `done` checkpoint, *then* it succeeds and the row is `done`.
*Given* the gate on,
*when* any actor runs `task add "w" --id t-y --status done`,
*then* it is refused with the DG-08 sentence 1 wording naming `t-y` and no row `t-y` exists afterwards.
*Given* the gate on,
*when* `import atmux-json` runs over a source containing a task-type row at `done`,
*then* it is refused with the DG-08 sentence 1 wording naming that row id and the whole import rolls back, so no row from the source lands.
*Given* a twin board where the gate was never turned on,
*when* each of the three writes runs there,
*then* each succeeds exactly as at the baseline (the checkpoint closes to `done`, the add lands `done`, the import lands the `done` row).

### A18 (DG-18 — the verdict list hides evidence the caller cannot read)

*Given* the gate on, task `t-x` carrying a tag the caller can read, a resolved attention row the caller cannot read (a tag outside its grants), and a stored verdict on `t-x` citing that row,
*when* the caller runs `task verdict list t-x`,
*then* the verdict row answers with its writer, reviewer, SHAs and verdict intact and the unreadable id absent from its evidence array; *when* a caller granted the hidden tag runs the same list, *then* the id is present.

## 5. Contracts and data

- **Interface version or schema:** `task move ID done --as ACTOR [--force]` CLI
  grammar, unchanged in shape; the gate adds refusal sentences (DG-08) and the
  DG-05 actor constraint on `--force`. New grammar: `task verdict add ID
  --reviewer ACTOR --sha SHA [--sha ...] --evidence ATTENTION-ID [--evidence ...]
  --as WRITER` (DG-12) and `task verdict gate on|off --as ACTOR` (DG-15).
- **Data invariants:** a row that reached `done` under the gate always carries,
  or is accompanied by, the latest stored planner-written foreign-actor `pass`
  verdict covering the current head and citing resolved attention decisions — or
  carries an override event naming `geoyws` (DG-06). A verdict's reviewer and
  writer are never the row's claim holder or closer (DG-03, DG-07, DG-12).
  Verdict rows are append-only; a refused write never moves the head (DG-14). Cited attention rows are
  `resolved` at the done-move (DG-04).
- **Migration:** new append-only `verdicts` table at the next `BOARD_V` ladder
  step above the tip (`BOARD_V37` above the `BOARD_V36` tip observed in this
  worktree, `rust/db.rs:2309`, ladder `rust/db.rs:3483`-`rust/db.rs:3484`),
  migrating forward only, with old boards reading as "no verdict" (gate refuses
  until one is earned, never backfilled). The `done_gate` flag needs no schema
  step: it is a `board_meta` key absent (off) until set. The override event MUST
  survive the migration path.
- **Compatibility:** ungated boards and older binaries behave as DG-09 states;
  pre-gate `done` rows gain no verdict fields. Stories and epics take no verdict
  (DG-16).
- **Ownership:** the tasks ledger owns the verdict and override records, the
  `done_gate` flag and the task head; the attention ledger owns the cited
  decision rows; the lanes own how a review is performed.

## 6. Quality and security

- **Reliability:** N/A — the gate is a synchronous refusal inside `move_task`'s
  existing transaction; no new retry, queue or background path.
- **Accessibility:** N/A — CLI-only surface, no rendered UI.
- **Privacy:** N/A — verdict and override records carry actor names and SHAs
  already present on the board; no new personal data.
- **Security:** a self-review or executor-written verdict MUST NOT be upgradeable
  into a pass by rewording (DG-03, DG-07 and DG-12 are actor/lease checks, not
  string checks); the override is confined to exactly `geoyws` (DG-05), matching
  the attention resolve/reopen precedent; unpublished SHAs fail closed (DG-13).
- **Operability:** every override is one auditable event (DG-06) and every flag
  change is one auditable event (DG-15); gate refusals name the holder- and
  force-shaped way out the way the lease refusal does
  (`rust/store.rs:2831`-`rust/store.rs:2838`).
- **Performance:** an observation is not yet measured — §7 OQ-6 owns it; no
  budget is set here.

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Where does the verdict live: new table, new `tasks` columns (`BOARD_V36`), or metadata JSON? | George | answered 2026-09-29 | none — pinned in DG-11: new append-only `verdicts` table |
| OQ-2 | What is the verdict-write grammar: new CLI subcommand, planner-only store call, or attention-like resolve path — and how is "spawned by the planner" attested on the ledger? | George | answered 2026-09-29 | none — pinned in DG-12: `task verdict add`, attested by actor and lease-hold (DG-07) |
| OQ-3 | What exactly is a "reviewed SHA": worktree diff hash, commit SHA, or board revision — and when does newer work after the verdict make it stale? | George | answered 2026-09-29 | none — pinned in DG-13 (full 40-char commit SHAs published on origin) and DG-14 (stale once a newer head is recorded; stale never opens the gate) |
| OQ-4 | How is the gate enabled: per-board flag, global default-on, or epic-scoped — and what is the default for existing boards? | George | answered 2026-09-29 | none — pinned in DG-15: per-board audited flag, off by default, acies first |
| OQ-5 | How does the gate compose with the story-status projection (`rust/store.rs:5400`-`rust/store.rs:5410`): does a story's `done` projection require a verdict per child story, per epic, or both? | George | answered 2026-09-29 | none — pinned in DG-16: per task only, stories and epics project done from children with no extra verdict |
| OQ-6 | What is the measured cost of the gate check on `task move` (observation, not a budget)? | slice implementer | open | rollout, not the spec gate |

## 8. Verification

Planned only — no test below runs today. Each row is owed by the implementation
row and lands in the matrix section `## Requirements trace — docs/specs/done-gate.md`.
Where there is no browser evidence the Note says `no e2e coverage` plainly; every
surface here is CLI, so every row does. Test names are proposed, not enumerated
(no implementation exists to list).

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| DG-01 | MUST | process | planned: `done_gate_refuses_done_move_without_verdict` | no e2e coverage |
| DG-02 | MUST | process | planned: `done_gate_rejects_incomplete_verdict_record` | no e2e coverage |
| DG-03 | MUST | process | planned: `done_gate_refuses_self_review_verdict` | self-review incl. writer-as-closer; A3 + A13; no e2e coverage |
| DG-04 | MUST | process | planned: `done_gate_requires_resolved_decision_citations` | no e2e coverage |
| DG-05 | MUST | process | planned: `done_gate_force_requires_geoyws` | no e2e coverage |
| DG-06 | MUST | process | planned: `done_gate_override_writes_audited_event` | no e2e coverage |
| DG-07 | MUST | process | planned: `done_gate_refuses_executor_written_verdict` | no e2e coverage |
| DG-08 | MUST | process | planned: `done_gate_refusals_carry_named_reasons` | seven verbatim sentences; no e2e coverage |
| DG-09 | MUST | process | planned: `done_gate_off_leaves_move_unchanged` | no e2e coverage |
| DG-10 | MUST | process | planned: `done_gate_fires_only_on_done` | no e2e coverage |
| DG-11 | MUST | process | planned: `done_gate_verdicts_table_is_append_only_across_migration` | new table, forward-only step; no e2e coverage |
| DG-12 | MUST | process | planned: `done_gate_verdict_add_verb_records_pass_only` | writer/reviewer holder check; no e2e coverage |
| DG-13 | MUST | process | planned: `done_gate_refuses_short_and_unpublished_shas` | sentences 5 and 6; no e2e coverage |
| DG-14 | MUST | process | planned: `done_gate_stale_verdict_does_not_open_gate` | sentence 2; no e2e coverage |
| DG-15 | MUST | process | planned: `done_gate_flag_defaults_off_and_audits_changes` | acies first; no e2e coverage |
| DG-16 | MUST | process | planned: `done_gate_story_and_epic_project_done_without_verdict` | no extra verdict; no e2e coverage |
| DG-17 | MUST | process | `a17_done_gate_checkpoint_done_is_gated`, `a17_done_gate_task_add_done_is_refused`, `a17_done_gate_import_done_is_refused` | each refuses with sentence 1 on the gated board with nothing written and succeeds on the ungated twin; no e2e coverage |
| DG-18 | MUST | process | `a18_done_gate_verdict_list_hides_unreadable_evidence` | tag-scoped managed estate: unreadable evidence ids omitted, readable rows intact; no e2e coverage |

## 9. Change log

- `2026-09-29` — created as PROPOSAL/BLOCKED; no requirements superseded.
- `2026-09-29` — independent review (BLOCKED) fixes: DG-14 pins the current head to the newest non-null `head_sha` across `checkpoints`, `task_claims`, `handoffs` and `sitreps` (A9 via checkpoint); DG-15 confines the toggle to `geoyws` with DG-08 sentence 7 (A10 covers the refused toggle); DG-02 stores the immutable writer and DG-03 refuses a writer-as-closer (new A13); DG-13 states the offline consequence.
- `2026-09-29` — George approved the slice (attention `a-741f1212`) and answered
  OQ-1..OQ-5; pinned as DG-11..DG-16 (new IDs at the end of the creation
  sequence, DG-01..DG-10 stable and unrenumbered). DG-01, DG-02, DG-06, DG-07
  and DG-09 gained cross-references to the new requirements without changing
  their obligations; DG-08 now pins the seven refusal sentences verbatim; §2
  drops the "no new write endpoint" non-goal (decided by OQ-2) and adds the
  no-fail-verdicts non-goal; status to `DRAFT`, gate requested — `SPEC-READY`
  awaits the independent reviewer. Gate-site citations re-read in this worktree
  at `6088ec9` (see §1 Sources); the migration step is named above the observed
  `BOARD_V36` tip.
- `2026-09-29` — review fixes: DG-14 pins head ordering by row time with the
  `task_claims`-first tie-break, heartbeat re-recording, the no-head rule,
  `fresh` as citing the current head, and exact-match comparison against
  abbreviated `--head` provenance (new A14, A15); DG-03 defines `claim holder
  of record` over the live claim row and claim-event history (new A16); §2
  qualifies the executor bar as authorship; §8 merges the writer-as-closer row
  into DG-03.
- `2026-09-29` — independent review round 3 (SPEC-READY): DG-16 corrected — only
  story rows are refused a direct move to `done` by the projection rule; epic rows
  move via `task move` at the baseline, so a direct epic move succeeds with no
  verdict (A11 covers both halves). Line citations re-verified in this worktree:
  DG-14 `looks_like_head_sha` to `rust/lib.rs:4101`; §1 gate-site lines to
  `rust/store.rs:5870` (lease seize), `rust/store.rs:5896` (`task_moved` event)
  and `rust/store.rs:5842`-`rust/store.rs:5852` (projection refusal, also §2 and
  DG-16). Status stamped `SPEC-READY`; no requirement ID changed meaning.
- `2026-09-29` — adversarial cover of the SPEC-READY slice (George's principle: the verdict path is the only way a task reaches done): new DG-17 gates every write that makes a task done (`task move`, `checkpoint --state done`, `task add --status done`, import; stories and epics stay DG-16, `transact`/`batch` inherit the same store methods) reusing the DG-08 sentences with no new wording (DG-08 byte-identical); new DG-18 omits unreadable evidence ids from `task verdict list`; DG-10 keeps its ID with a dated pointer to DG-17 for path coverage; new A17, A18 and §8 rows.
