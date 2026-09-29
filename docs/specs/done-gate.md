# Specification: a done move needs a foreign-actor review verdict (slice DG)

## 1. Identity and baseline

- **Slice ID:** `DG`. Requirement IDs are `DG-01` .. `DG-10`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-29` at commit `f8e1719` on branch `wt/t-7038c70a-spec`. Every
  "today" claim below cites the line that has it, as `<path>:<line>`.
- **Status:** `PROPOSAL/BLOCKED` pending slice approval (board row `t-01feac30` under
  epic `e-c0852fe7`, approver George). This draft authorises no implementation, no
  rollout and no release.
- **Owner (product scope):** George. He alone resolves scope, the gate's enablement
  mechanism, and whether the verdict lives in a new table or in task metadata.
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
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 names the rows
  this slice will add when it is approved; this specification does not restate the
  matrix.

## 2. Purpose and scope

**Intended outcome.** A task row reaches `done` only through a reviewer who did
not do the work: with the gate on, the ledger refuses a done-move that carries no
foreign-actor pass verdict, so the incident — a row closed as implemented with no
reviewer and 7 defects found later — cannot recur by the same path.

**Users / actors.**

- Executor lane (claim holder): does the work, holds the lease, moves the row
  toward done. It can never satisfy the gate for its own row.
- Reviewer lane: spawned by the planner/main loop, works from kb itself (row,
  linked attention decisions, pinned diff), records the verdict. It never holds
  the work lease it reviews.
- Planner / main loop: spawns executor and reviewer, writes the verdict the
  reviewer earned to the ledger. The only writer of verdict records.
- George (`geoyws`): the only actor who may override the gate past a refusal, and
  whose override is audited.

**In scope.** `task move ID done` under the gate: the verdict record, the
foreign-actor rule, the named refusal, the `geoyws`-only `--force` override and
its audit event, the planner-writes-verdict authorship rule.

**Boundaries.**

- The reviewer's working method (what it reads, how it diffs) is owned by the
  lanes that run it, not by this slice; this slice pins only what the ledger
  demands and records.
- Attention resolve/reopen actor rules are owned by their existing specifications;
  this slice cites them as precedent and as the evidence the verdict points at.
- The story-status projection (`rust/store.rs:5400`-`rust/store.rs:5410`) is owned
  by the story slice; §7 OQ-5 owns the interaction.

**Non-goals.**

- **No retroactive verdicts.** Shipped rows that reached `done` without a verdict
  acquire none; the gate governs moves made while it is on, not history.
- **No review quality metric.** The slice pins that a foreign actor checked named
  decisions at a named SHA, not that the check was good; judging reviews is
  George's, not the ledger's.
- **No new write endpoint for verdicts.** The verdict arrives through the
  mechanism §7 OQ-2 decides (CLI subcommand or planner-only store call); this
  slice does not invent a second review tool.
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
With the gate on, `task move ID done` on a row with no review-verdict record
satisfying DG-02 .. DG-04 and DG-07 is refused. The refusal carries the named
reason DG-09 states and leaves the row, its lease and the event history
unchanged.
*Permissions:* applies to every actor including the claim holder and including
`geoyws` without `--force`; DG-06 is the only way past it.

**DG-02** — require the verdict record's four fields.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 1.
A verdict record carries exactly: reviewer actor, reviewed SHA(s), verdict
`pass`, and evidence. A record missing any of the four, with an empty reviewer,
an empty SHA list, or a verdict other than `pass`, does not satisfy DG-01.
*Data rules:* the record is durable ledger state and survives a restart; it is
never silently dropped by a later move, retag or metadata patch.

**DG-03** — refuse a verdict whose reviewer is the worker.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask point 1.
The reviewer actor MUST differ from the claim holder of record for the reviewed
work and from the closing actor attempting the done-move. A verdict naming the
holder or the closer as reviewer is refused as if no verdict existed (DG-01),
with the refusal naming the collision.
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
`rust/store.rs:7186`-`rust/store.rs:7195`.
`task move ID done --force` past an unsatisfied gate is refused unless the acting
actor is exactly `geoyws` (`rust/model.rs:846`). Any other actor with `--force`
receives the refusal and the row is unchanged.

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
*Failure behaviour:* an executor-written verdict is refused with a named reason;
no verdict record is stored and DG-01 still refuses the done-move.

### Refusal wording and ungated boards

**DG-08** — refuse in the product's own sentence.
Strength: MUST · Layer: process · Source: ADR-008 (refusal wording is product
contract); `rust/store.rs:2831`-`rust/store.rs:2838` (lease-refusal precedent).
Each gate refusal states the one reason in fixed wording: `no pass verdict`
(missing/incomplete record), `self-review` (DG-03 collision),
`verdict not by planner loop` (DG-07), `override is geoyws-only` (DG-05), or
`stale verdict` (reviewed SHA no longer covers the row — see §7 OQ-3). The exact
sentences are pinned when the slice is approved; until then no implementation may
hard-code them.

**DG-09** — leave ungated boards exactly unchanged.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` acceptance.
With the gate off, `task move ID done` behaves byte-for-byte as at the baseline
(`rust/store.rs:5379`-`rust/store.rs:5465`): no verdict is demanded, no new
refusal is reachable, and boards written before the gate carry no verdict fields
and need none.

**DG-10** — gate only the move to done.
Strength: MUST · Layer: process · Source: kb `t-7038c70a` ask (done-gate, not a
work-gate).
The gate fires only on moves to status `done`. Moves to `in_progress`, `review`,
`blocked`, `todo`, `backlog`, `draft` or `cancelled` never consult verdict state,
and the prerequisite gate (`rust/store.rs:5417`-`rust/store.rs:5427`) is
otherwise unchanged.

## 4. Acceptance examples

### A1 (DG-01, DG-03, DG-08)

*Given* the gate on, task `t-x` claimed and worked by executor `lane-e`, with no
verdict record,
*when* `lane-e` runs `task move t-x done --as lane-e`,
*then* the move is refused with the `no pass verdict` reason, and the row status,
lease and event count are unchanged.

### A2 (DG-02, DG-03, DG-04, DG-07)

*Given* the gate on, `t-x` worked by `lane-e`, reviewer `lane-r` spawned by the
planner, attention rows `a-1` and `a-2` resolved against `t-x`, and a planner-
written verdict `{reviewer lane-r, SHAs [pinned diff SHA], pass, evidence [a-1,
a-2]}`,
*when* any actor runs `task move t-x done`,
*then* the move succeeds, the row is `done` with `completed_at` set, and the
verdict record is still attached to the row.

### A3 (DG-03 — self-review refused)

*Given* the gate on and a verdict on `t-x` naming reviewer `lane-e`, who holds or
held the work claim,
*when* `lane-e` runs `task move t-x done --as lane-e`,
*then* the move is refused with the `self-review` reason and the row is unchanged.

### A4 (DG-05, DG-06 — geoyws-only audited override)

*Given* the gate on and `t-x` with no verdict,
*when* `geoyws` runs `task move t-x done --as geoyws --force`,
*then* the move succeeds and one override event exists naming `geoyws`, `t-x`,
the prior status and the unsatisfied-gate reason; *when* `lane-e` runs the same
with `--as lane-e --force`, *then* it is refused with the `override is
geoyws-only` reason and the row is unchanged.

### A5 (DG-07 — executor cannot write the verdict)

*Given* the gate on and `t-x` claimed by `lane-e`,
*when* `lane-e` attempts to write a `pass` verdict for `t-x`,
*then* the write is refused with the `verdict not by planner loop` reason, no
verdict is stored, and a later `task move t-x done --as lane-e` is still refused.

### A6 (DG-09, DG-10 — ungated boards and non-done moves unchanged)

*Given* the gate off and `t-y` with no verdict,
*when* its holder runs `task move t-y done` and separately `task move t-y review`,
*then* both succeed exactly as at the baseline with no new refusal reachable and
no verdict field required.

## 5. Contracts and data

- **Interface version or schema:** `task move ID done --as ACTOR [--force]` CLI
  grammar, unchanged in shape; the gate adds refusal reasons (DG-08) and the
  DG-05 actor constraint on `--force`. Verdict write grammar is §7 OQ-2 (undecided
  — no grammar is pinned here).
- **Data invariants:** a row that reached `done` under the gate always carries,
  or is accompanied by, a planner-written foreign-actor `pass` verdict citing
  resolved attention decisions — or carries an override event naming `geoyws`
  (DG-06). A verdict's reviewer is never the row's claim holder or closer
  (DG-03). Cited attention rows are `resolved` at the done-move (DG-04).
- **Migration:** undecided — §7 OQ-1. If the verdict is a new table or new
  columns, the slice needs a `BOARD_V36` ladder step (ladder at `BOARD_V35` per
  `rust/db.rs:3134`-`rust/db.rs:3140`) migrating forward only, with old boards
  reading as "no verdict" (gate refuses until one is earned, never backfilled).
  If it is metadata JSON, no migration is needed and this line will say so.
- **Compatibility:** ungated boards and older binaries behave as DG-09 states;
  pre-gate `done` rows gain no verdict fields. The override event MUST survive
  the migration path §7 OQ-1 selects.
- **Ownership:** the tasks ledger owns the verdict and override records; the
  attention ledger owns the cited decision rows; the lanes own how a review is
  performed.

## 6. Quality and security

- **Reliability:** N/A — the gate is a synchronous refusal inside `move_task`'s
  existing transaction; no new retry, queue or background path.
- **Accessibility:** N/A — CLI-only surface, no rendered UI.
- **Privacy:** N/A — verdict and override records carry actor names and SHAs
  already present on the board; no new personal data.
- **Security:** a self-review or executor-written verdict MUST NOT be upgradeable
  into a pass by rewording (DG-03, DG-07 are actor/lease checks, not string
  checks); the override is confined to exactly `geoyws` (DG-05), matching the
  attention resolve/reopen precedent.
- **Operability:** every override is one auditable event (DG-06); gate refusals
  name the holder- and force-shaped way out the way the lease refusal does
  (`rust/store.rs:2831`-`rust/store.rs:2838`).
- **Performance:** an observation is not yet measured — §7 OQ-6 owns it; no
  budget is set here.

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Where does the verdict live: new table, new `tasks` columns (`BOARD_V36`), or metadata JSON? | George | open | implementation (SPEC-READY) |
| OQ-2 | What is the verdict-write grammar: new CLI subcommand, planner-only store call, or attention-like resolve path — and how is "spawned by the planner" attested on the ledger? | George | open | implementation (SPEC-READY) |
| OQ-3 | What exactly is a "reviewed SHA": worktree diff hash, commit SHA, or board revision — and when does newer work after the verdict make it stale? | George | open | implementation (SPEC-READY) |
| OQ-4 | How is the gate enabled: per-board flag, global default-on, or epic-scoped — and what is the default for existing boards? | George | open | implementation (SPEC-READY) |
| OQ-5 | How does the gate compose with the story-status projection (`rust/store.rs:5400`-`rust/store.rs:5410`): does a story's `done` projection require a verdict per child story, per epic, or both? | George | open | implementation (SPEC-READY) |
| OQ-6 | What is the measured cost of the gate check on `task move` (observation, not a budget)? | slice implementer | open | rollout, not SPEC-READY |

## 8. Verification

Planned evidence — no test names below exist yet; they are enumerated with
`cargo test -- --list` before landing, and this table is copied into
`docs/testing/compiled-rust-e2e-matrix.md` as the slice's trace section when the
slice is approved. Until then every row is a plan, not a claim.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| DG-01 | MUST | process | `done_gate_refuses_done_move_without_verdict` | planned; no e2e coverage |
| DG-02 | MUST | process | `done_gate_rejects_incomplete_verdict_record` | planned; no e2e coverage |
| DG-03 | MUST | process | `done_gate_refuses_self_review_verdict` | planned; no e2e coverage |
| DG-04 | MUST | process | `done_gate_requires_resolved_decision_citations` | planned; no e2e coverage |
| DG-05 | MUST | process | `done_gate_force_requires_geoyws` | planned; no e2e coverage |
| DG-06 | MUST | process | `done_gate_override_writes_audited_event` | planned; no e2e coverage |
| DG-07 | MUST | process | `done_gate_refuses_executor_written_verdict` | planned; no e2e coverage |
| DG-08 | MUST | process | `done_gate_refusals_carry_named_reasons` | planned; no e2e coverage |
| DG-09 | MUST | process | `done_gate_off_leaves_move_unchanged` | planned; no e2e coverage |
| DG-10 | MUST | process | `done_gate_fires_only_on_done` | planned; no e2e coverage |

## 9. Change log

- `2026-09-29` — created as PROPOSAL/BLOCKED; no requirements superseded.
