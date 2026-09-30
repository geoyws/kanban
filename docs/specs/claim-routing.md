# Specification: a lane executor claims its lane's rows whatever harness label the assignee carries (slice CLAIM)

## 1. Identity and baseline

- **Slice ID:** `CLAIM`. Requirement IDs are `CLAIM-01` .. `CLAIM-08`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-29` at commit `ab7a11a` on branch `kanban-geoyws-driver`. Every "today"
  claim below cites the line that has it, as `<path>:<line>`, read in this worktree. Board schema
  at the baseline is `36` (`rust/db.rs:2920`), and no `lane_of` function exists yet: the two claim
  paths compare the whole assignee string to `--as` (see Sources). The specification is written
  before the implementation, as ADR-047 §6 requires.
- **Status:** `SPEC-READY` on 2026-09-29. An independent `/quality spec` review (subagent
  `ClaimRoutingSpecReview`, which did not write this document) read commit `5e12247`, checked
  every `path:line` citation against the code, and found one non-blocking finding: the matrix
  preamble said the rows were "proved" at `process` while every row is still planned. That wording
  is fixed. This stamp claims specification readiness only; product readiness after
  implementation stays `/quality`, then `/tidy`. No served-tier evidence is owed: the slice
  changes no served markup.
- **Owner (product scope):** George. He alone resolves scope, the open questions in §7, and
  whether a non-goal in §2 is reinstated. He approved this slice under epic `e-c0852fe7` on
  attention `a-4a6388df` (board row `t-9c195580`); the implementation row is `t-8c698a02`.
- **Decider (wording of this document):** `@:geoyws/kanban/driver`, the lane that owns
  `t-9c195580`. Where this document and the approved row body differ on a fact, the row body wins
  and this document is corrected.
- **Sources:**
  - **George, 2026-09-17:** "only a different LANE is another worker; every lane executor claims
    its lane's rows whatever harness label the assignee carries." This rule is the slice approval
    ADR-047 §7 requires for a slice outside the `WEB`/`SPA` rollout, and `t-9c195580` sits under
    epic `e-c0852fe7` for that reason.
  - **Measured 2026-09-28, px driver-2** (row `t-9c195580` body): a row assigned to
    `@:px/px/driver-2` is hidden from the same lane asking as `claude@driver-2` — 0 claimable
    instead of 7.
  - Shipped surface at the baseline: `eligible_claim_candidates` (`rust/store.rs:3835`), whose
    assignee gate reads `value == agent` (`rust/store.rs:3909`-`rust/store.rs:3913`); the named
    claim's refusal `task {id} is assigned to {assignee}` (`rust/store.rs:6980`-`rust/store.rs:6982`);
    the claim write that retargets the assignee to the caller's `--as`
    (`rust/store.rs:6997`-`rust/store.rs:7000`); the `ClaimOptions` fields `caller_lane`,
    `cross_lane` and `allow_reassign` (`rust/store.rs:3647`-`rust/store.rs:3655`); the lane
    preference that filters candidates on the task `lane` column
    (`rust/store.rs:3919`-`rust/store.rs:3942`); the CLI grammar `claim [ID | --next] --as AGENT`
    with `[--lane LANE] [--no-cross-lane] [--allow-reassign]`
    (`rust/lib.rs:152`-`rust/lib.rs:162`); the lane-word grammar `driver_lane_name` (`rust/store.rs:45`-`rust/store.rs:56`) and the
    bare/typed parser `driver_lane_address` both sides of the handoff cutover share
    (`rust/store.rs:64`-`rust/store.rs:80`).
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §6 is why this slice is specified before implementation, §9 is why
    §6 carries no invented budget.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why a typed form
    naming a different team or board is a different worker (CLAIM-03), and why every refusal
    below writes nothing.
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section `## Requirements trace — docs/specs/claim-routing.md` is the trace
  of record when it lands (convention at
  `docs/testing/compiled-rust-e2e-matrix.md:206`-`docs/testing/compiled-rust-e2e-matrix.md:218`).

## 2. Purpose and scope

**Intended outcome.** A lane executor claims its own lane's rows no matter which harness label the
stored assignee carries, and is still refused rows that belong to a different lane — so the same
worker asking as `claude@driver-2` sees the rows assigned to `@:px/px/driver-2`.

**Users / actors.**

- **Lane executors and their harnesses** — claim with `--as` in whatever spelling their harness
  uses (bare `driver-2`, harness `claude@driver-2`, typed `@:px/px/driver-2`), and receive the
  existing refusal only for another lane's rows.
- **The board operator (George)** — files rows with any assignee spelling; stored spellings are
  never rewritten by this slice.

**In scope.** The one lane-identity function (`lane_of`); the assignee gate in
`eligible_claim_candidates` (`rust/store.rs:3909`-`rust/store.rs:3913`), which drives both
`claim --candidates` and `claim --next`; the assignee gate on the named-claim path
(`rust/store.rs:6980`-`rust/store.rs:6982`).

**Boundaries.** The task `lane` column, `--lane`/`--role`/`--no-cross-lane` filtering, the
`driver-only` check, the model allow-list, sprint scoping, and tag authorization keep their own
rules and their own place in the ladder; this slice inserts no check and moves none. The handoff
acceptance path (`rust/store.rs:9078`-`rust/store.rs:9088`) is untouched: it already has its own
legacy lane match.

**Non-goals.**

- Renaming stored assignees. No migration touches any stored string.
- Changing what a claim writes. The claim still retargets the assignee to the caller's `--as`
  verbatim (CLAIM-08).
- A registry of lanes or harnesses. Lane spelling is parsed, never looked up.
- This slice does not define a performance, latency, availability or retention target.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

Every requirement below is proven at `process`: each is observable at the CLI across a real
process boundary, because the e2e layer spawns the compiled binary. There is no browser surface
to drive, so no row carries browser evidence.

### Lane identity

**CLAIM-01** — Map the three lane spellings to one lane token.
Strength: `MUST` · Layer: `process` · Source: George, 2026-09-17; measured 2026-09-28.
`lane_of` takes one identity string and returns either a lane token or no token. A lane word is
exactly what the shipped `driver_lane_name` accepts (`rust/store.rs:45`-`rust/store.rs:56`):
`driver`, or `driver-` followed by a number with no leading zero; `driverless`, `driver-two` and
`driver-0` are not lane words. `lane_of` returns a lane token for exactly three shapes, compared
byte-exact and case-sensitive:

- a bare lane word: `driver-2` yields `driver-2`; `driver` yields `driver`;
- a typed long form, as the shipped `driver_lane_address` parses it
  (`rust/store.rs:64`-`rust/store.rs:80`): `@:` then exactly three non-empty `/`-segments whose
  last is a lane word, so `@:px/px/driver-2` yields `driver-2`;
- a harness form: a string that does not begin `@:`, holds exactly one `@`, has a non-empty part
  before it, and has a lane word after it, so `claude@driver-2` yields `driver-2` and
  `codex@driver` yields `driver`.

The three shapes yield the same token for the same lane. Anything else yields no token
(CLAIM-02). No other normalization happens: no trimming, no lowercasing.

**CLAIM-02** — Fall back to exact-string comparison where there is no lane.
Strength: `MUST` · Layer: `process` · Source: George, 2026-09-17 (conservative reading).
Where either side yields no lane token — a plain name (`geoyws`), `superdriver` (it matches no
lane-word grammar at `rust/store.rs:45`-`rust/store.rs:56`, so it is an ordinary identity), or a
typed form whose final segment is not a lane word — the two sides are the same worker only when
the two strings are byte-identical. A lane spelling and a lane-less string are never the same
worker: `driver-2` does not match `geoyws`.

**CLAIM-03** — Treat a typed form naming a different team or board as a different worker.
Strength: `MUST` · Layer: `process` · Source: ADR-008 (fail closed).
When both sides are typed (both begin `@:`) and their team or board segments differ, they are
different workers even when the lane tokens are equal: `@:px/px/driver-2` and
`@:other/kanban/driver-2` refuse each other. When at most one side is typed, only the lane token
decides. This is the conservative reading of "only a different LANE is another worker": the
team/board names the estate that owns the lane address, and an ambiguous address refuses rather
than hands a row to another estate's worker (OQ-1 records the relaxation George may order).

### Claim routing

**CLAIM-04** — Show a same-lane row among the caller's own candidates.
Strength: `MUST` · Layer: `process` · Source: George, 2026-09-17; measured 2026-09-28.
In `eligible_claim_candidates` (`rust/store.rs:3909`-`rust/store.rs:3913`) a candidate whose
assignee is the caller's own row under `lane_of` (CLAIM-01..CLAIM-03) is routable exactly as an
unassigned row or a row assigned to the caller's whole string is today. The measured case:
a row assigned to `@:px/px/driver-2` appears in `claim --candidates --as claude@driver-2` and
may be selected by `claim --next --as claude@driver-2`. `--lane`, `--role`, `--no-cross-lane`,
`driver-only`, model, sprint and tag filters apply before and after this gate unchanged.

**CLAIM-05** — Let a named claim take a same-lane row.
Strength: `MUST` · Layer: `process` · Source: George, 2026-09-17; measured 2026-09-28.
On the named-claim path (`rust/store.rs:6980`-`rust/store.rs:6982`) a task whose assignee is the
caller's own row under `lane_of` proceeds past the assignee check exactly as a task assigned to
the caller's whole string does. The refusal order is unchanged: the assignee check still sits
after the model check.

**CLAIM-06** — Refuse a different lane's row with the existing sentence.
Strength: `MUST` · Layer: `process` · Source: ADR-008; George, 2026-09-17.
A task whose assignee is another worker under `lane_of` is refused on the named-claim path with
the byte-identical sentence the baseline writes: `task {id} is assigned to {assignee}`, naming
the stored assignee verbatim. The refusal writes nothing: no lease row, no `task_claimed` event,
no status change. The row is likewise absent from the caller's `--candidates` and never selected
by the caller's `--next`.

**CLAIM-07** — Leave `--allow-reassign` exactly as it is.
Strength: `MUST` · Layer: `process` · Source: shipped surface at the baseline.
`--allow-reassign` still bypasses the assignee gate for every assignee spelling, on the candidate
path and on the named-claim path alike (`rust/store.rs:3913`, `rust/store.rs:6980`). It gains no
lane meaning and loses none. The help sentence at `rust/lib.rs:327` ("only filters claim
--candidates") speaks about taking a row off a live lease, which `--allow-reassign` never does;
this slice does not touch that sentence.

**CLAIM-08** — Keep the claim's retarget write verbatim and unnormalized.
Strength: `MUST` · Layer: `process` · Source: shipped surface at the baseline.
A successful claim still stores the caller's `--as` string byte-for-byte as the task's assignee
(`rust/store.rs:6997`-`rust/store.rs:7000`): it does not canonicalize to the lane token, the
harness form, or the typed form. Stored spellings therefore keep their variety across claims;
only the comparison is lane-aware. Retargeting to a canonical spelling is out of scope (§2).

## 4. Acceptance examples

Every invocation below runs against the compiled binary, with `--as` on every write, and is
executable as written once the implementation lands. `T` is a `todo` task with no `lane` column
set, so no `--lane`/`--role` filter removes it; nothing else on the board is claimable.

### A1 (`CLAIM-04`, `CLAIM-01`)

*Given* `kb task add "Sweep the logs" --assignee @:px/px/driver-2 --as geoyws --json` filed task
`T`,
*when* a lane runs `kb claim --candidates --as claude@driver-2 --json`,
*then* `T` is listed; at the baseline the same command listed nothing (0 claimable instead of 7
on the measured board).

### A2 (`CLAIM-05`, `CLAIM-01`, `CLAIM-08`)

*Given* task `T` assigned to `@:px/px/driver-2`,
*when* a lane runs `kb claim T --as claude@driver-2 --json`,
*then* the claim succeeds, and `kb task show T --json` reads `"assignee": "claude@driver-2"` —
the caller's own string stored verbatim, not canonicalized to the lane token.

### A3 (`CLAIM-05`, `CLAIM-01`)

*Given* task `T` assigned to `@:px/px/driver-2`,
*when* lanes run `kb claim T --as driver-2 --json` and separately
`kb claim T --as codex@driver --json` on a fresh board each time,
*then* the bare lane succeeds; the `codex@driver` claim is refused with
`task T is assigned to @:px/px/driver-2`, because `driver` and `driver-2` are different lanes.

### A4 (`CLAIM-06`, `CLAIM-02`)

*Given* task `T` assigned to `@:px/px/driver-2`,
*when* a lane runs `kb claim T --as claude@driver-3 --json`, and the operator runs
`kb claim T --as geoyws --json` and `kb claim --candidates --as geoyws --json`,
*then* every claim exits non-zero with `task T is assigned to @:px/px/driver-2`, the candidates
list omits `T`, `T` stays `todo` with its assignee unchanged, and no `task_claimed` event was
appended.

### A5 (`CLAIM-03`, `CLAIM-06`)

*Given* task `T` assigned to `@:px/px/driver-2`,
*when* a lane runs `kb claim T --as @:other/kanban/driver-2 --json`,
*then* the claim exits non-zero with `task T is assigned to @:px/px/driver-2`: both sides are
typed and the team/board differ, so the same lane token still refuses.

### A6 (`CLAIM-07`, `CLAIM-02`)

*Given* task `T` assigned to `@:px/px/driver-2`,
*when* a lane runs `kb claim T --as claude@driver-3 --allow-reassign --json`, and separately
`kb claim --candidates --as claude@driver-3 --allow-reassign --json`,
*then* the named claim succeeds and the candidates list contains `T`: `--allow-reassign` still
bypasses the gate for any spelling. And *given* a task `U` assigned to `superdriver`,
*when* a lane runs `kb claim U --as claude@driver-2 --json`,
*then* the claim is refused with `task U is assigned to superdriver`: `superdriver` carries no
lane, so only its exact string matches.

## 5. Contracts and data

- **Interface version or schema:** N/A — no new route, verb, subcommand, flag or field. The CLI
  grammar at `rust/lib.rs:152`-`rust/lib.rs:162` is unchanged; `lane_of` is an internal
  comparison, not surface.
- **Data invariants:** stored assignee strings are never normalized, rewritten or migrated by
  this slice; equality changes only in memory at claim time. Unassigned rows stay claimable by
  any caller, exactly as the `is_none_or` arm reads today
  (`rust/store.rs:3909`-`rust/store.rs:3913`).
- **Migration:** none. No board schema version bump; a board at schema 36 opens unchanged.
- **Compatibility:** an older client that passes the same `--as` spellings observes a wider pool
  (same-lane rows now routable) and fewer refusals (same-lane named claims now succeed); every
  refusal it still receives carries the sentence it already knows.
- **Ownership:** the task rows and their assignees stay owned by the existing store contracts;
  this slice owns only the comparison.

## 6. Quality and security

- **Reliability:** N/A — the slice adds no new failure mode: the two gates keep their fail-closed
  refusals and write nothing on refusal.
- **Accessibility:** N/A — no browser or rendered surface changes.
- **Privacy:** N/A — assignee strings are already visible to every caller who may read the row;
  the slice exposes no new value.
- **Security:** the different-lane refusal stays non-enumerating where the board already is:
  the sentence names only the row the caller already named, and candidate filtering stays silent
  (a skipped row produces no refusal). No authorization check moves.
- **Operability:** N/A — no migration to retry, no new flag to misspell.
- **Performance:** observation only, not a budget — the gates remain per-row string comparisons
  over the already-fetched candidate pool; no timing commitment is made (see ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Should two typed forms with equal lane tokens but different team/board segments count as the same worker (pure lane-token equality)? | George | open — this document takes the conservative, fail-closed default in `CLAIM-03` (they refuse). Relaxing it later is a supersession of `CLAIM-03`, not a reinterpretation | none — the default is safe to implement; relaxing it only widens what `CLAIM-03` refuses today |
| OQ-2 | Is `superdriver` a lane for claim routing? | George | open — this document takes the conservative default: no. `superdriver` is not a lane word (`rust/store.rs:45`-`rust/store.rs:56`), so it falls under `CLAIM-02`'s exact-string rule, as every other lane-less actor does | none — the default changes nothing the board does today for `superdriver` |

Neither question is material to `SPEC-READY`: each default is the behaviour the board already has
for that case, so implementing the slice under the default loses nothing George could later want,
and a relaxation arrives as a supersession with its own change-log entry.

## 8. Verification

Every test name below is planned, not existing: each is marked `planned`, no name is claimed to
exist at the baseline, and no name was enumerated with `cargo test -- --list` because the
implementation row `t-8c698a02` has not run yet. Each is a compiled-binary exchange at `process`,
landing in the `## Requirements trace — docs/specs/claim-routing.md` matrix section on
implementation. The matrix carries `none`/`none` rows with `no e2e coverage` until implementation
enumerates the real names; this table is the draft it copies, and the deliberate difference is
stated on both sides rather than hidden.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `CLAIM-01` | `MUST` | `process` | planned: `claim_routing_treats_bare_harness_and_typed_spellings_as_one_lane` | asserts A1–A3: the three spellings yield one token; `no e2e coverage` |
| `CLAIM-02` | `MUST` | `process` | planned: `claim_routing_falls_back_to_exact_strings_without_a_lane` | asserts the `geoyws` and `superdriver` halves of A4/A6; `no e2e coverage` |
| `CLAIM-03` | `MUST` | `process` | planned: `claim_routing_refuses_a_typed_lane_from_another_board` | asserts A5; `no e2e coverage` |
| `CLAIM-04` | `MUST` | `process` | planned: `claim_candidates_show_same_lane_rows_to_the_callers_own_lane` | asserts A1 and the A6 candidates half; `no e2e coverage` |
| `CLAIM-05` | `MUST` | `process` | planned: `named_claim_takes_a_same_lane_row` | asserts A2–A3; `no e2e coverage` |
| `CLAIM-06` | `MUST` | `process` | planned: `named_claim_refuses_a_different_lane_in_the_existing_words` | asserts A4–A5 byte-identical sentence and the unwritten board; `no e2e coverage` |
| `CLAIM-07` | `MUST` | `process` | planned: `allow_reassign_still_bypasses_every_assignee_spelling` | asserts A6; `no e2e coverage` |
| `CLAIM-08` | `MUST` | `process` | planned: `successful_claim_stores_the_caller_string_verbatim` | asserts the A2 `task show` read; `no e2e coverage` |

No `chrome` or `http` evidence is planned: the slice adds no browser or HTTP surface, so
process-boundary exchanges prove what the slice states; the matrix rows say `no e2e coverage`
plainly per the convention at `docs/testing/compiled-rust-e2e-matrix.md:206`-`:218`.

## 9. Change log

- `2026-09-29` — slice created at `CLAIM-01` .. `CLAIM-08`. No supersessions yet.
- `2026-09-29` — `SPEC-READY` after independent review; matrix preamble reworded from "proved" to
  "planned" (review finding 1). No requirement changed.
- `2026-09-29` — implemented by `t-8c698a02`: `claim_lane`/`same_claim_worker` in `rust/store.rs`
  (the `lane_of` of `CLAIM-01`) and the eight §8 tests, now real names in the matrix. No
  requirement changed.
