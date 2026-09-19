# Specification: a sprint is a typed boundary that cannot close without a served version (slice SPRINT)

## 1. Identity and baseline

- **Slice ID:** `SPRINT`. Requirement IDs are `SPRINT-01` .. `SPRINT-39`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-19` at commit `d15822d` on branch `kanban-geoyws-driver`. Every "today"
  claim below cites the line that has it, as `<path>:<line>`. Served state on 2026-09-19: `hax`
  and `hig` run release `461b1c1` at board schema 28, which carries this slice; the baseline
  commit `d15822d` (schema 29) is in its gate. A fact, not a requirement.
- **Status:** `SPEC-READY` on 2026-09-19 (independent gate by a reviewer applying the SDD §1 exit criteria over two rounds; three findings closed — owners on the five `none` rows, `--as` on every §4 invocation, the dated served-state fact in §1). It authorises neither implementation nor release.
  <!-- SPEC-READY is stamped here by an independent reviewer against the SDD reference's §1 exit
  criteria, not by the writer of this document. It authorises neither implementation nor rollout
  nor release. -->
- **Owner (product scope):** George. He alone resolves scope, whether a deliberately-unbuilt
  behaviour (§2 Non-goals) is reinstated, and the open questions in §7.
- **Decider (wording of this document):** the `t-64fb4ae7` writer.
- **Sources:**
  - `docs/adr/ADR-045-sprints-are-proof-gated-version-boundaries.md` — the decision this
    specification states as requirements. §1 (the typed row and its closed status set), §2 (seven
    verbs, the flags that reach existing commands, and the refusals that name their fix), §3
    (claims are sprint-scoped by default and crossing is recorded), §4 (done means served), §5
    (the `dash`/`ctx` projections and the three web pages) and §6 (six event kinds, one chain)
    are binding on every requirement below.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why every refusal
    quoted below names its fix, and why an ambiguous pair of flags is refused rather than ranked.
  - `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` — the chain the
    six sprint event kinds join.
  - `docs/adr/ADR-030-deployment-attempt-ledger-and-self-archiving.md` — the deployment rows the
    close gate reads; a receipt is a note, not proof.
  - `docs/adr/ADR-037-listings-are-bounded-and-say-so.md` — the bound on `sprint list`.
  - `docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md` §2 — the
    schema-versus-store enforcement precedent `task_sprints` follows, and §7, the reason
    `abandon` requires a note.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §9 is why §6 carries no invented budget.
  - `docs/PRD.md:73`-`docs/PRD.md:81` (`### Ship a sprint to a served version`) and
    `docs/PRD.md:203`-`docs/PRD.md:239` (`### Sprint release boundaries`) — the product-level
    statement these requirements refine.
  - Epic `e-ee95dbd5` — ADR-045's stated authority for the sprint requirement source.
  - Shipped surface at the baseline: `rust/lib.rs:223`-`rust/lib.rs:231` (the seven
    `sprint` subcommands in `COMMANDS`), `rust/lib.rs:133` (`task add --sprint`),
    `rust/lib.rs:151` (`task update --sprint | --clear-sprint`), `rust/lib.rs:158` and
    `rust/lib.rs:182` (`claim` and `handoff accept` overrides), `rust/lib.rs:105`
    (`deploy start --sprint`), `rust/lib.rs:118` (`deploy finish --served-version`),
    `rust/model.rs:884` (`SPRINT_STATUSES`), `rust/model.rs:887` (`sprint_id`),
    `rust/model.rs:1441` (`target_version`), `rust/model.rs:1468` (`sprint_goal`),
    `rust/model.rs:1921` (the `Sprint` row), `rust/db.rs:1750` (`BOARD_V27`),
    `rust/db.rs:1767` (the `one_current_sprint` partial unique index), `rust/db.rs:1786`
    (`BOARD_V28`), `rust/db.rs:2294` (`BOARD_SCHEMA_VERSION`, `29` at this commit),
    `rust/store.rs:7695` (`create_sprint`), `rust/store.rs:7733` (`plan_sprint`),
    `rust/store.rs:7829` (`start_sprint`), `rust/store.rs:7907` (`close_sprint`),
    `rust/store.rs:8026` (`abandon_sprint`), `rust/store.rs:8058` (`sprints`),
    `rust/store.rs:8105` (`sprint_tasks`), `rust/store.rs:8125` (`sprint_task_counts`),
    `rust/store.rs:2422` (`require_attachable_sprint_on`), `rust/store.rs:2499`
    (`resolve_claim_sprint`), `rust/store.rs:7075` (`context_sprint`),
    `rust/store.rs:7594`-`rust/store.rs:7604` (the served-version gate on `deploy finish`),
    `rust/lib.rs:5946`-`rust/lib.rs:5971` (`dash`'s `currentSprint`), `rust/serve.rs:595`-
    `rust/serve.rs:597` and `rust/serve.rs:630`-`rust/serve.rs:632` (the three sprint routes),
    `rust/serve.rs:2783` (`sprints`), `rust/serve.rs:2802` (`board_sprints`),
    `rust/serve.rs:2814` (`sprint_detail`), `rust/search.rs:551`-`rust/search.rs:553` (the
    citation shape).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section is the trace of record and the two are kept identical by the same
  change.

## 2. Purpose and scope

**Intended outcome.** A board carries one durable row that says "this is the sprint, it ships as
version X.Y.Z, and it is not done until that version is observed serving" — so a lane cannot
claim past a release boundary that was never crossed, and nobody can call a release done on a
receipt instead of on a served version.

**Users / actors.**

- **The board operator (George)** — opens, plans, starts, closes and abandons the boundary from
  the CLI, and reads it on `kb dash`, `kb sprint list`/`show` and the three served sprint pages.
- **Lane agents** — claim, accept handoffs and read `kb ctx <task>` inside the boundary, or cross
  it on purpose with a flag that says so in `kb ev`.
- **The deploy mechanism** — starts a deployment attempt bound to the sprint, observes the served
  version independently of the sprint row, and records that observation on the attempt; the
  attempt is the only thing the close gate will read.

**In scope.**

- The seven `sprint` subcommands — `new`, `plan`, `start`, `close`, `abandon`, `list`, `show`
  (`rust/lib.rs:223`-`rust/lib.rs:231`) — and the sprint flags on the commands they reach:
  `task add --sprint`, `task update --sprint | --clear-sprint`, `claim --sprint | --any-sprint`,
  `handoff accept --sprint | --any-sprint`, `deploy start --sprint`,
  `deploy finish --served-version`.
- Sprint-scoped claims and the deliberate cross: with a `current` sprint the candidate pool is
  that sprint's rows and nothing else — an unattached row is outside the boundary, not inside it
  — with two recorded overrides.
- The close gate: a close reads one succeeded verification-phase deployment attempt bound to this
  sprint whose target and independently observed served version both equal the sprint's, and
  carry-over of unfinished rows to a named destination with a written note, in the same
  transaction as the close.
- The two CLI projections (`kb dash`'s `currentSprint`, `kb ctx <task>`'s sprint block) and the
  three read-only server-rendered pages `/sprints`, `/sprints/{project}`, `/sprint/{project}/{id}`.
- The six event kinds — `sprint_created`, `sprint_planned`, `sprint_started`, `sprint_closed`,
  `sprint_abandoned`, `task_sprint_changed` — on the one ADR-029 hash chain.
- The `BOARD_V27` and `BOARD_V28` migrations as they run on a real v26 board, and the sprint rows
  they add to first-class search.

**Boundaries.**

- **The deployment ledger** is ADR-030's, not this slice's. This slice adds three columns to it
  (`sprint_id`, `target_version`, `served_version`) and one equality rule at `deploy finish`; the
  attempt lifecycle, its capability token, its phases and its self-archiving stay owned there.
- **Sprint-scoped rules** (`SPRINT:sp-ID` selectors, `rule add/update --sprint`,
  `rule update --clear-sprint`) are ADR-045 §5 behaviour implemented in the registry
  (`rust/registry.rs:303`-`rust/registry.rs:333`, `rust/registry.rs:358`-`rust/registry.rs:387`)
  and specified with the rule surface, not here. This slice owns only the authoritative
  attachment they are evaluated against.
- **Search ranking and fusion** are the retrieval slice's. This slice owns that a sprint is an
  indexed source with a stable citation shape, refreshed by its own lifecycle.
- **The served pages' design system** is ADR-046's, preserved here by citation, never by copy.
- **Authorization** is the store's, board-scoped: a sprint row carries no tags of its own
  (`rust/store.rs:7707`), so a caller who may write the board may write the sprint, and the rows
  a caller may not read contribute neither content nor count.

**Non-goals.** ADR-045's Consequences names these as deliberately not built, and this
specification does not add them:

- **Velocity.** No rate of completed work per sprint is computed or stored.
- **Points or estimates.** A sprint row and a task row carry no size field; the projections count
  rows, not effort.
- **Burndown.** No time series of remaining work exists; `dash` publishes the current open and
  done counts and days remaining from `scheduled_end`, and nothing historical.
- **Rollover automation.** Unfinished work never moves by itself: a close either has nothing
  unfinished or is refused until the operator names the destination sprint and writes the note.
- **Scheduling automation.** Nothing starts, closes or archives a sprint because wall-clock time
  passed; `scheduled_start`/`scheduled_end` are planned boundaries read by projections, and
  `starts_at`/`ends_at` are stamped only by the verbs.
- **No change to lease, heartbeat or checkpoint semantics.** An override is recorded on the claim
  event; the token, the expiry and the heartbeat are byte-identical to a board with no sprint.
- **No invented commitment.** §6 records the one ADR-037 bound that exists and no latency,
  availability or retention number; §7 carries what would otherwise be a guess.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced values or bytes, not a browser.
- `http` — a compiled-binary HTTP exchange against `kanban serve`, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

IDs are assigned in creation order and never reused; the groups below are topical. Every refusal
quoted below is the product's own sentence at the baseline, read from the line cited beside it —
changing one of those sentences is a product change (ADR-008, `docs/specs/README.md`).

### The sprint row and its identity

**SPRINT-01** — a sprint is a row in its own table under a typed `sp-` identity.
Strength: MUST · Layer: process · Source: ADR-045 §1.
A sprint is a row in `sprints` (`rust/db.rs:1751`), never a row in `tasks`. Its id is
operator-suppliable with `--id` and otherwise minted as `sp-` plus eight hex characters
(`rust/store.rs:7709`-`rust/store.rs:7712`). An id that is not of that shape is refused with
`sprint id must start with sp-, include a suffix, be at most 64 ASCII characters, and contain
only letters, digits, dot, underscore, or hyphen` (`rust/model.rs:896`-`rust/model.rs:898`), and
no row, no event and no attachment is written.
*Data rules:* the row survives a restart in the board's own SQLite file; `sprint list` and
`sprint show` read it back with the `SPRINT_FIELDS` projection (`rust/lib.rs:4164`).

**SPRINT-02** — a target version that nobody can match against a served artifact is refused.
Strength: MUST · Layer: unit · Source: ADR-045 §2.
`--target-version` is `X.Y.Z` with an optional `-suffix` and is validated before the row exists
(`rust/model.rs:1441`). Anything else is refused with `version <value> must read X.Y.Z with an
optional -suffix, such as 0.4.0 or 0.4.0-rc.1 — the version is matched against a served artifact,
so a shape nobody can match is decoration` (`rust/model.rs:1460`-`rust/model.rs:1462`), quoting
the offered value. The target version is distinct from the global `--version` banner: passing
sprint arguments never consumes it.

**SPRINT-03** — the four statuses are a published closed set.
Strength: MUST · Layer: process · Source: ADR-045 §1.
The status vocabulary is `planned`, `current`, `closed`, `abandoned` — declared once in
`SPRINT_STATUSES` (`rust/model.rs:884`), enforced by the row's own `CHECK`
(`rust/db.rs:1755`-`rust/db.rs:1756`), and published through `schema --json`. A
`sprint list --status` value outside the set is refused by a sentence that names the whole set
(`rust/store.rs:8060`), so the reader learns the vocabulary from the refusal.

**SPRINT-04** — the planned window is a window.
Strength: MUST · Layer: none · Source: ADR-045 §1.
`--start` and `--end` are planned epoch milliseconds. A negative `--start`, or an `--end` before
`--start`, is refused with `sprint schedule requires non-negative --start and --end at or after
--start` (`rust/store.rs:7700`) before the write opens, and the schema holds the same rule
independently as `CHECK(scheduled_end >= scheduled_start)` (`rust/db.rs:1759`). A non-integer
value for either flag is refused as `--start must be an epoch-millisecond integer`
(`rust/lib.rs:7277`).

**SPRINT-05** — the actual lifecycle stamps are distinct from the planned ones.
Strength: MUST · Layer: unit · Source: ADR-045 §1, §2.
`starts_at` is written as the sentinel `0` at creation (`rust/store.rs:7715`) and overwritten
with the real start time by `sprint start` (`rust/store.rs:7872`); `ends_at` is `NULL` until
`close` or `abandon` stamps it (`rust/store.rs:8002`, `rust/store.rs:8040`);
`closed_by_deployment` is written only by `close`, and names the attempt that proved the version.
A reader therefore always distinguishes what was planned from what happened: `sprint_detail`
prints `not started` for the sentinel rather than an epoch (`rust/serve.rs:2820`-
`rust/serve.rs:2823`).

**SPRINT-06** — a sprint is not work.
Strength: MUST · Layer: none · Source: ADR-045 §1 ("Not the tasks table").
A sprint is never claimable, never gated and never handed off. Observably: `sprints` carries no
assignee, lease, claim or gate column (`rust/db.rs:1751`-`rust/db.rs:1766`); no `sprint`
subcommand mints or accepts a lease token (`rust/lib.rs:223`-`rust/lib.rs:231`); and `kb claim`,
`kb handoff` and the story gate reach tasks only, so `kb claim sp-…` is answered by the
task-not-found refusal rather than by a lease.
*Data rules:* the one-sprint-per-task relation lives in `task_sprints`, whose primary key is the
task (`rust/db.rs:1769`); the `tasks` table is not rebuilt and carries no sprint column.

### The seven verbs and the refusals that name their fix

**SPRINT-07** — a board has at most one current sprint, and the refusal names the holder.
Strength: MUST · Layer: unit · Source: ADR-045 §1, §2 refusal 1.
Starting a sprint while another is `current` is refused with `sprint sp-a is current; close or
abandon it before starting sp-b` — naming both the holder and the row that was refused
(`rust/store.rs:7864`-`rust/store.rs:7869`). The refused row stays `planned`, and the partial
unique index `one_current_sprint` (`rust/db.rs:1767`) holds the same rule against a bug in the
check: when it fires, the same sentence is produced from the holder the index names
(`rust/store.rs:7879`-`rust/store.rs:7889`), so the rule cannot fail open from either side.

**SPRINT-08** — planning records a deliberate scope or is refused.
Strength: MUST · Layer: process · Source: ADR-045 §2.
`sprint plan` accepts scope exactly four ways: `--candidate ID ...`, `--parent-epic e-…`,
scope already explicitly attached to this sprint, or the explicit `--empty-scope` decision. With
none of them it is refused with `sprint plan requires --candidate, --parent-epic, existing
explicit scope, or --empty-scope` (`rust/store.rs:7787`-`rust/store.rs:7789`). `--empty-scope`
combined with candidates, a parent epic or existing scope is refused with `--empty-scope cannot
be combined with candidate, parent epic, or existing sprint scope`
(`rust/store.rs:7782`-`rust/store.rs:7784`) — two answers to one question, refused rather than
ranked. `--parent-epic` pointing at a non-epic is refused naming the type it found
(`rust/store.rs:7767`-`rust/store.rs:7770`).
*Data rules:* a refused plan writes nothing — not the body, not an event, not an attachment.

**SPRINT-09** — planning without a goal is refused, not stored as an empty card.
Strength: MUST · Layer: none · Source: ADR-045 §2.
`sprint plan` requires `--body` or `--body-file` and refuses with `sprint plan requires --body or
--body-file: the goal and success criteria are the plan — without them the sprint is a title and
a version` (`rust/lib.rs:7292`-`rust/lib.rs:7294`). A body that is present but blank is refused
as `sprint body is required` (`rust/store.rs:7742`).

**SPRINT-10** — starting requires a recorded goal and a recorded scope decision.
Strength: MUST · Layer: process · Source: ADR-045 §2.
`sprint start` refuses a sprint with no goal headline in its body with `sprint sp-x has no
recorded goal and criteria; run sprint plan first` (`rust/store.rs:7843`), a sprint that has no
`sprint_planned` event with `sprint sp-x has not been planned; run sprint plan first`
(`rust/store.rs:7847`), and a planned sprint whose scope is neither attached nor deliberately
empty with `sprint sp-x has no deliberate scope; plan candidates, a parent epic, or
--empty-scope before starting` (`rust/store.rs:7858`-`rust/store.rs:7860`). The goal headline is
the body's first non-blank line (`rust/model.rs:1468`).

**SPRINT-11** — closed and abandoned sprints are history, and history is not rewritten.
Strength: MUST · Layer: unit · Source: ADR-045 §2 refusal 6.
`plan`, `start`, `close` and `abandon` each refuse a `closed` sprint with `sprint sp-x is closed
history; its card cannot be rewritten` (`rust/store.rs:7748`, `rust/store.rs:7834`,
`rust/store.rs:7923`, `rust/store.rs:8033`). An `abandoned` sprint is refused by the verb that
names the way forward: `sprint sp-x is abandoned; open a new sprint instead of rewriting this one
— the goal it never met is part of the record` on `plan` (`rust/store.rs:7752`-
`rust/store.rs:7754`), `…instead of starting this one` on `start` (`rust/store.rs:7837`),
`sprint sp-x is abandoned; it cannot be closed` on `close` (`rust/store.rs:7924`), and `sprint
sp-x is already abandoned` on a second `abandon` (`rust/store.rs:8036`). A `current` sprint is
refused replanning with `sprint sp-x is current and cannot be replanned; close or abandon it
before planning another sprint` (`rust/store.rs:7757`-`rust/store.rs:7759`), and a `planned` one
is refused closing with `sprint sp-x is planned, not current; start it before closing it`
(`rust/store.rs:7925`).
*Data rules:* every one of these refusals leaves the row's body, status and event count
unchanged.

**SPRINT-12** — abandoning states its reason.
Strength: MUST · Layer: unit · Source: ADR-045 §2, ADR-042 §7.
`sprint abandon` moves any non-closed sprint to `abandoned`, stamps `ends_at`, and requires
`--note TEXT`: a blank or absent note is refused with `abandon note is required`
(`rust/store.rs:8027`). The note is recorded on the `sprint_abandoned` event payload
(`rust/store.rs:8048`), because "later" with no stated reason is how a boundary silently stops
existing.

**SPRINT-13** — the default sprint listing answers "what is live or coming", and says when it is
cut.
Strength: MUST · Layer: process · Source: ADR-045 §2, ADR-037.
`sprint list` orders by `created_at DESC, id DESC` (`rust/store.rs:8078`). With no `--status` and
no `--all` it shows only `planned` and `current` rows (`rust/store.rs:8075`) and only unarchived
ones (`rust/store.rs:8066`); `--all` includes closed, abandoned and archived history. The
listing is bounded at the ADR-037 default of 100 (`rust/lib.rs:7336`): a board with more sprints
than the default is refused rather than silently truncated, with `found more than 100 sprints
and no --limit was given — a page cut at the default would read as the whole; pass --limit N,
above 100 to see more or exactly 100 to take the first 100 knowingly`
(`rust/lib.rs:2187`-`rust/lib.rs:2191`).

**SPRINT-14** — `sprint show` resolves the boundary, its rows and its proof in one read.
Strength: MUST · Layer: process · Source: ADR-045 §2, §5.
`sprint show ID` answers the row, the task rows attached to it, and the deployment row named by
`closed_by_deployment` resolved for the reader — `null` when the sprint has not closed
(`rust/lib.rs:7345`-`rust/lib.rs:7358`). Attached rows come back in the listing's own order,
`priority, created_at, id` (`rust/store.rs:8111`). An unknown id is refused with `sprint sp-x
does not exist on this board` (`rust/store.rs:2404`).
*Data rules:* `list` and `show` are reads and write nothing — no event, no stamp, no archival.

**SPRINT-15** — one task sits in at most one sprint, and attaching a container carries its
subtree.
Strength: MUST · Layer: process · Source: ADR-045 §2.
A task's attachment is a single row in `task_sprints` keyed by the task
(`rust/db.rs:1768`-`rust/db.rs:1773`), written at creation with `task add --sprint sp-…` and
later with `task update --sprint sp-… | --clear-sprint` (`rust/lib.rs:133`, `rust/lib.rs:151`).
Attaching a row attaches the row itself plus every descendant that is not already in another
sprint; a descendant that moved itself elsewhere stays where it put itself — the subtree is one
recursive query and the moved set is that one predicate
(`rust/store.rs:2466`-`rust/store.rs:2477`). Detaching is explicit and non-recursive: it clears
exactly the named row (`rust/store.rs:5038`) and refuses an unattached one with `task t-x is not
attached to a sprint; there is nothing to clear` (`rust/store.rs:5036`).
*Failure behaviour:* naming a sprint that does not exist refuses the whole command and rolls it
back — after a refused `task add --sprint sp-missing` the task does not exist either.
*Data rules:* every attachment change records who attached it and when (`attached_by`,
`attached_at`, `rust/store.rs:2486`-`rust/store.rs:2487`) and is audited: `task add --sprint`
emits one `task_sprint_changed` subjected to the new row and naming the moved set
(`rust/store.rs:4494`-`rust/store.rs:4500`), while `task update --sprint | --clear-sprint`
(`rust/store.rs:5044`-`rust/store.rs:5052`) and `sprint plan`
(`rust/store.rs:7801`-`rust/store.rs:7807`) emit one event per moved row, subjected to that row,
so event visibility hides restricted descendants without losing lineage.

**SPRINT-16** — history is not a boundary rows can be attached to.
Strength: MUST · Layer: unit · Source: ADR-045 §2.
An attachment naming a `closed` sprint is refused with `sprint sp-x is closed history; attach the
rows to a planned or current sprint instead` and an `abandoned` one with `sprint sp-x is
abandoned; attach the rows to a planned or current sprint instead`
(`rust/store.rs:2422`-`rust/store.rs:2432`). This is the same gate the carry-over destination
passes through, so work is never bounded by a sprint that already ended. A closed sprint remains
readable, remains a valid `--sprint` claim override (SPRINT-17) and remains a valid rule
selector.

### Sprint-scoped claims and the deliberate cross

**SPRINT-17** — the current sprint is the candidate pool, and unattached is outside it.
Strength: MUST · Layer: unit · Source: ADR-045 §3.
When a board has a `current` sprint, `claim --candidates` and `claim --next` offer only rows
whose attachment is that sprint. An unattached row is **not** offered — the sprint is the
boundary, and unattached is outside it, not inside. The two overrides are `--sprint sp-…`, which
offers exactly that sprint's rows, and `--any-sprint`, which offers everything the board would
have offered with no sprint at all. `--sprint` naming an unknown sprint is refused with `sprint
sp-x does not exist on this board` and an abandoned one with `sprint sp-x is abandoned`
(`rust/store.rs:2510`-`rust/store.rs:2515`); a **closed** sprint is deliberately accepted,
because landing a fix against the boundary that just shipped is what the override exists for.
*Data rules:* one resolver serves the read-only inspection and the atomic writer
(`rust/store.rs:2499`), so `claim --candidates` can never show a row `claim --next` refuses.

**SPRINT-18** — a named claim across the boundary is refused with the way across.
Strength: MUST · Layer: process · Source: ADR-045 §3.
`claim <id>` for a row outside the boundary is refused with `task t-x is not in sprint sp-a; use
--sprint with its sprint or --any-sprint to cross the current boundary explicitly`
(`rust/store.rs:5087`-`rust/store.rs:5089`). No lease is minted, no status moves and no event is
appended.

**SPRINT-19** — crossing the boundary is recorded, and staying inside it changes nothing.
Strength: MUST · Layer: process · Source: ADR-045 §3.
A claim that used `--sprint sp-…` records `sprintOverride: "sp-…"` on its `task_claimed` event
payload and one that used `--any-sprint` records `sprintOverride: "any"`, visible in `kb ev`
(`rust/store.rs:5160`-`rust/store.rs:5166`). A default-scoped claim records no such key at all:
the payload is byte-identical to a board that has no sprints.

**SPRINT-20** — handoff acceptance obeys the same boundary as a claim.
Strength: MUST · Layer: process · Source: ADR-045 §3, §4.
Accepting a handoff mints a lease, so it resolves the boundary through the same resolver and
refuses a cross-boundary acceptance with the same sentence as SPRINT-18
(`rust/store.rs:6579`-`rust/store.rs:6591`), and records the same `sprintOverride` on its own
event payload (`rust/store.rs:6635`-`rust/store.rs:6636`). The lease token, the heartbeat and the
expiry are unchanged by any of this.

**SPRINT-21** — one answer to the boundary question.
Strength: MUST · Layer: none · Source: ADR-045 §2 refusal 5, ADR-008.
`--sprint` and `--any-sprint` on the same invocation are refused with `--sprint and --any-sprint
both answer which sprint boundary this claim crosses; pass one — --sprint names the boundary,
--any-sprint says any` (`rust/lib.rs:2488`-`rust/lib.rs:2489`), before the board is opened for
writing. The refusal applies at each of the three sites that accept the pair: `claim
--candidates` (`rust/lib.rs:6490`), `claim` (`rust/lib.rs:6929`) and `handoff accept`
(`rust/lib.rs:7047`).

**SPRINT-22** — a board with no current sprint behaves exactly as it did before sprints existed.
Strength: MUST · Layer: unit · Source: ADR-045 §3.
With no `current` sprint the filter is a no-op: the candidate SQL is the statement it always was,
because the sprint clause is appended only when a boundary exists
(`rust/store.rs:2553`-`rust/store.rs:2557`); every row the board would have offered is offered,
including unattached ones; and the claim event grows no key. A merely `planned` sprint is not a
boundary — the pool is untouched until one is `current`.

### Done means served

**SPRINT-23** — a close names a deployment attempt or it is refused.
Strength: MUST · Layer: process · Source: ADR-045 §4.
`sprint close` without `--deployment` is refused with `sprint close requires --deployment d-…:
the version is served or the sprint is not done` (`rust/store.rs:7915`-`rust/store.rs:7917`). A
`--deployment` naming no row is refused with `deployment d-x not found`
(`rust/store.rs:7937`).

**SPRINT-24** — only a succeeded verification-phase attempt is proof.
Strength: MUST · Layer: process · Source: ADR-045 §4, ADR-030.
An attempt that is not `succeeded` at phase `verification` is refused with `sprint sp-a cannot
close on d-x: that attempt is <status> (<phase>); a sprint closes only on a succeeded
verification-phase deployment`, quoting the status and phase it found and `no phase` where the
attempt has none (`rust/store.rs:7938`-`rust/store.rs:7945`).
*Data rules:* the gate reads the attempt's typed columns only. The receipt text is never parsed
and is not version proof — a receipt that names the version satisfies nothing.

**SPRINT-25** — the proof must be this sprint's, at this sprint's version.
Strength: MUST · Layer: process · Source: ADR-045 §4.
Close is refused unless the attempt's `sprint_id`, `target_version` and independently observed
`served_version` all equal this sprint's id and target version, with `sprint sp-a cannot close on
d-x: proof must be bound to this sprint and served version X.Y.Z`
(`rust/store.rs:7946`-`rust/store.rs:7953`). A succeeded verification attempt for another
sprint, or for this sprint at another version, closes nothing.

**SPRINT-26** — the deployment attempt is bound to the sprint when it starts, and must observe
the bound version to succeed.
Strength: MUST · Layer: process · Source: ADR-045 §4.
`deploy start --sprint sp-…` resolves the sprint and stores both its typed id and its target
version on the attempt row in the same transaction (`rust/db.rs:1774`-`rust/db.rs:1776`). A
bound attempt finishing `succeeded` at `verification` requires `--served-version`: absent, it is
refused with `deployment d-x is bound to sprint sp-a target version X.Y.Z; successful
verification requires --served-version X.Y.Z`; unequal, with `deployment d-x is bound to sprint
sp-a target version X.Y.Z, but served version Y was observed`
(`rust/store.rs:7594`-`rust/store.rs:7601`). `--served-version` on an unbound attempt is refused
with `--served-version requires a deployment started with --sprint`, and on a non-succeeded one
with `--served-version is proof for a succeeded verification deployment only`
(`rust/store.rs:7603`-`rust/store.rs:7608`).

**SPRINT-27** — unfinished work is carried by name, with a note, in the same transaction as the
close.
Strength: MUST · Layer: process · Source: ADR-045 §4.
If any attached row is outside `done`/`cancelled` at close, the close requires both `--carry-to
sp-…` and a non-empty `--carry-note TEXT`, refusing with `sprint sp-a has N unfinished row(s);
close requires --carry-to sp-… and --carry-note TEXT` (`rust/store.rs:7980`) and, with the
destination but no note, `carry-over note is required` (`rust/store.rs:7981`). The destination
must be a different, attachable sprint (SPRINT-16), refusing `carry-over destination must be a
different sprint` (`rust/store.rs:7983`). Authorization checks over every attached row, the
attachment moves, the per-row `task_sprint_changed` events carrying the note, and the
`sprint_closed` event commit atomically (`rust/store.rs:7988`-`rust/store.rs:8020`): after a
refused close nothing moved, and after an accepted one the destination holds the rows at their
own unchanged statuses.

**SPRINT-28** — carry flags with nothing to carry are refused rather than ignored.
Strength: MUST · Layer: none · Source: ADR-045 §4, ADR-008.
When every attached row is `done` or `cancelled`, passing `--carry-to` or `--carry-note` is
refused with `sprint sp-a has no unfinished rows to carry over` (`rust/store.rs:7976`) rather
than accepted as a no-op, because a carry-over that silently did nothing reads exactly like one
that worked.

### Projections

**SPRINT-29** — the dashboard publishes the boundary an operator needs without being asked.
Strength: MUST · Layer: process · Source: ADR-045 §5.
`kb dash` carries a `currentSprint` object for the board's current sprint with its id, title,
status, `targetVersion`, `scheduledStart`, `scheduledEnd`, `daysRemaining` (ceiling days from
`scheduled_end`, floored at zero), the visible `open` and `done` counts, the `goal` headline and
the body (`rust/lib.rs:5946`-`rust/lib.rs:5971`). A board with no current sprint carries no
`currentSprint` key at all.

**SPRINT-30** — task context names the release only when the task is in it.
Strength: MUST · Layer: process · Source: ADR-045 §5.
`kb ctx <task>` for an attached task carries a `sprint` object with `sprintID`, `status`,
`targetVersion`, `title` and the `goal` headline (`rust/store.rs:7091`-`rust/store.rs:7097`),
and its text rendering carries the `sp-… "title" vX.Y.Z · status` line and `Goal: <headline>`.
For an unattached task the JSON field is absent and the text is the text it was before sprints
existed — no `## Sprint` section.
*Data rules:* the packet's task read and its sprint read come from one snapshot, so a packet
never mixes two boards' moments.

**SPRINT-31** — three read-only server-rendered sprint pages exist, and they are reads.
Strength: MUST · Layer: unit · Source: ADR-045 §5, ADR-016.
`render` answers `/sprints` across every registered board, `/sprints/{project}` for one board and
`/sprint/{project}/{id}` for one sprint (`rust/serve.rs:595`-`rust/serve.rs:597`,
`rust/serve.rs:630`-`rust/serve.rs:632`). The board page carries the current sprint's card marked
`data-sprint-current`, with `data-sprint-version`, `data-sprint-state`, `data-sprint-open`,
`data-sprint-done` and the scheduled dates, then planned and historical sprints beneath it; an
empty board says `No current sprint. Planned and historical sprints remain below.` or `This board
has no sprints.` (`rust/serve.rs:2754`, `rust/serve.rs:2763`). The detail page adds the actual
start and end, the archived flag, the visible attached rows, and — for a closed sprint — the
`data-sprint-deployment-proof` card naming the served version and served commit, or
`No served deployment proof is attached.` (`rust/serve.rs:2858`-`rust/serve.rs:2878`).
*Permissions:* these pages are reads. No sprint verb is reachable over HTTP; the write allowlist
is unchanged by this slice.

**SPRINT-32** — the sprint pages are reachable and readable on the phone.
Strength: MUST · Layer: chrome · Source: ADR-045 §5, ADR-046.
From the served navigation, `Sprints` reaches `/sprints`, a board link reaches
`/sprints/{project}` and a sprint link reaches `/sprint/{project}/{id}`, with the seeded sprint's
version and goal on screen; none of the three routes overflows sideways at 390, 820 or 1280 CSS
px.

**SPRINT-33** — a sprint projection shows only what its reader may see, and never a capability.
Strength: MUST · Layer: unit · Source: ADR-045 §5, ADR-016.
Rows the caller may not read contribute neither content nor aggregate existence: `sprint_tasks`
drops them (`rust/store.rs:8118`) and `sprint_task_counts` counts only permitted rows
(`rust/store.rs:8140`-`rust/store.rs:8142`), so the served card's `open`/`done`, `sprint show`'s
rows and `dash`'s counts agree with what the reader is allowed to know. No sprint projection —
CLI, context packet or served page — carries a lease token: the sprint row has no such field and
the attached rows are projected without one.

### Events, and the chain they join

**SPRINT-34** — six event kinds, one chain, and `audit verify` stays green across the lifecycle.
Strength: MUST · Layer: process · Source: ADR-045 §6, ADR-029.
`sprint_created`, `sprint_planned`, `sprint_started`, `sprint_closed`, `sprint_abandoned` and
`task_sprint_changed` are written by the existing `event()` writer inside the row's own
transaction (`rust/store.rs:7718`, `rust/store.rs:7813`, `rust/store.rs:7892`,
`rust/store.rs:8006`, `rust/store.rs:8043`, `rust/store.rs:7995`), so they join the one
hash-chained journal. The close payload records `deploymentID`, `targetVersion` and
`previousStatus`; abandon records its `note`; an attachment change records `oldSprintID` and
`newSprintID`, and a carry records the `carryNote` beside them. After a full lifecycle
`audit verify` reports the board healthy.

**SPRINT-35** — the audit trail is the scope, so only real moves are recorded.
Strength: MUST · Layer: unit · Source: ADR-045 §2, §6.
`sprint plan` emits one `task_sprint_changed` per row that actually moved, each carrying the
`oldSprintID` it came from and `planned: true` (`rust/store.rs:7798`-`rust/store.rs:7808`); a row
already attached to this sprint produces no event, and a refused plan produces none at all. The
event count is therefore a count of changes, not of mentions.

### Schema and migration

**SPRINT-36** — V27 adds the sprint model additively to a real v26 board, and fails closed.
Strength: MUST · Layer: unit · Source: ADR-045 §1.
`BOARD_V27` (`rust/db.rs:1750`) is ordinary additive DDL — two `CREATE TABLE`s, one partial
unique index and three `ALTER TABLE deployments ADD COLUMN`s — with no `IF NOT EXISTS`, because
an unexpected object is incompatible state rather than evidence that the migration already ran.
Applied to a board built at the real v26 ladder it preserves every existing row; meeting an
object it did not expect, it refuses and rolls back with no partial schema.
*Data rules:* `task_sprints` carries no CHECK beyond its foreign keys — the attachment rules are
cross-row and live in the store, where a refusal can name the fix (ADR-042 §2).

**SPRINT-37** — a v26 board that migrates can complete a proof-gated lifecycle immediately.
Strength: MUST · Layer: process · Source: ADR-045 §1, Consequences.
A board constructed at schema 26 with a live pre-sprint row, opened by the compiled binary,
migrates in place, keeps that row readable, and then runs `sprint new` → `plan` → `start` →
bound `deploy start`/`finish` → `close` to a `closed` status, after which `audit verify` reports
the board and its journal healthy.

**SPRINT-38** — a sprint is a first-class search source with a stable citation.
Strength: MUST · Layer: process · Source: ADR-045 §5.
`BOARD_V28` (`rust/db.rs:1786`) widens the search document's source set to include `sprint`
(`rust/db.rs:1795`) and indexes a sprint's title, body and target version. A hit cites
`kanban://BOARD/sprint/ID` (`rust/search.rs:551`-`rust/search.rs:553`), the document is
refreshed by every lifecycle write through the source triggers
(`rust/db.rs:1959`-`rust/db.rs:1970`), and results stay board-scoped — a query on one board never
returns another board's sprint.
*Data rules:* sprint rows are board-only, carry no tags, and are not automatically archived.

**SPRINT-39** — V28 backfills existing sprints without rekeying or erasing what the index already
holds.
Strength: MUST · Layer: unit · Source: ADR-045 §5.
The V28 rebuild copies all fifteen `search_documents` columns explicitly, so row identities
(`seq`), `source_hash` and cached embeddings survive; the external-content FTS table is rebuilt
from those preserved rows after the triggers and indexes are restored; and existing sprint rows
are backfilled through the extended source view (`rust/db.rs:1786`-`rust/db.rs:1974`).

## 4. Acceptance examples

Given/When/Then in plain prose. Each heading names the requirement IDs it proves.

### A1 — opening a sprint (SPRINT-01, SPRINT-02, SPRINT-05, SPRINT-08, SPRINT-09, SPRINT-10)

*Given* an initialized board with one `todo` task `t-in` and no sprint,
*when* the operator runs `sprint new "Ship 1.2.3" --id sp-current --target-version 1.2.3 --start 0
--end 4102444800000 --as operator`, then `sprint start sp-current --as operator` before planning
it, then `sprint plan sp-current --body "Ship 1.2.3\nAcceptance" --candidate t-in --as operator`,
then `sprint start sp-current --as operator` again,
*then* the row exists as `sp-current` at status `planned` with `starts_at` `0`; the first `start`
is refused with `sprint sp-current has no recorded goal and criteria; run sprint plan first` and
the row is still `planned`; the plan attaches `t-in` and records one `task_sprint_changed` and one
`sprint_planned`; and the second `start` moves the row to `current` with `starts_at` stamped to a
real time. *And* a `sprint new` whose `--id` is `not-a-sprint` is refused naming the `sp-` rule
and writes no row, and one whose `--target-version` is `v1` is refused by the `X.Y.Z` sentence
quoting `"v1"`.

### A2 — a second current sprint is refused (SPRINT-07, SPRINT-11)

*Given* `sp-current` current and `sp-next` planned with a deliberately empty scope,
*when* the operator runs `sprint start sp-next --as operator`,
*then* the refusal names both rows — `sprint sp-current is current; close or abandon it before
starting sp-next` — `sp-next` is still `planned`, and no event was appended; *and when* the
operator instead replans `sp-current`, *then* that is refused with `sprint sp-current is current
and cannot be replanned; close or abandon it before planning another sprint` and the body and the
event count are unchanged.

### A3 — claiming inside and outside the boundary (SPRINT-17, SPRINT-18, SPRINT-19, SPRINT-20, SPRINT-22)

*Given* a board with `sp-current` current holding `t-in`, another sprint holding `t-other`, and an
unattached `t-loose`,
*when* a lane runs `claim --candidates --as worker` and then `claim --next --as worker`,
*then* the pool is exactly `[t-in]` — `t-loose` is not offered — and `--next` takes `t-in` with no
`sprintOverride` on its `task_claimed` event; *when* the lane runs `claim t-loose --as worker`,
*then* it is refused with `task t-loose is not in sprint sp-current; use --sprint with its sprint
or --any-sprint to cross the current boundary explicitly` and no lease exists; *when* it runs
`claim t-other --as worker --sprint sp-other`, *then* the claim succeeds and the event records
`sprintOverride: "sp-other"`; *when* it runs `claim t-loose --as worker --any-sprint`, *then* the
claim succeeds and the event records `sprintOverride: "any"`; *when* a handoff on `t-loose` is
accepted with `handoff accept h-… --as next`, *then* acceptance is refused by the same sentence
and succeeds as `handoff accept h-… --as next --any-sprint`; *and* on a board whose only sprint
is `planned`, every one of these commands behaves exactly as it did before sprints existed and
the claim payload grows no key.

### A4 — a close with no proof (SPRINT-23, SPRINT-24, SPRINT-25)

*Given* `sp-current` current at target version `5.0.0`,
*when* the operator runs `sprint close sp-current --as operator`,
*then* it is refused with `sprint close requires --deployment d-…: the version is served or the
sprint is not done`; *when* he names a `failed` verification attempt, *then* the refusal reads
`… that attempt is failed (verification); a sprint closes only on a succeeded
verification-phase deployment`; *when* he names a succeeded attempt at phase `start`, *then* the
refusal names that phase instead; *when* he names a succeeded verification attempt bound to
another sprint, *then* the refusal reads `… proof must be bound to this sprint and served version
5.0.0`; and after all four the sprint is still `current` with `closed_by_deployment` unset.

### A5 — the served version, and the close that carries (SPRINT-26, SPRINT-27, SPRINT-34)

*Given* `sp-current` current at `1.2.3` holding a `done` row and an unfinished `t-carry`, and
`sp-next` planned,
*when* the operator runs `deploy start --repo geoyws/kanban --commit FULL_SHA --tier @_p
--environment production --host hax --url https://kb.geoy.ws --sprint sp-current --as operator`,
finishes it with `deploy finish d-… --token TOKEN --result succeeded --phase verification
--receipt "observed running tier" --served-version 9.9.9 --as operator` and then the same
command with `--served-version 1.2.3`, then runs `sprint close sp-current --deployment d-… --as
operator`, then the same close with `--carry-to sp-next --carry-note "unfinished work is
explicitly deferred"`,
*then* the attempt carries `sprintID: sp-current` and `targetVersion: 1.2.3` from the start; the
`9.9.9` finish is refused naming `target version 1.2.3`; the `1.2.3` finish succeeds; the close
without carry flags is refused naming `--carry-to`; and the carrying close returns status
`closed` with `closedByDeployment` set, moves `t-carry` to `sp-next` at its own unchanged status,
records `task_sprint_changed` with `oldSprintID`, `newSprintID` and the `carryNote`, records
`sprint_closed` with `deploymentID`, `targetVersion` and `previousStatus`, and leaves
`audit verify` healthy.

### A6 — abandoning (SPRINT-12, SPRINT-11)

*Given* a `planned` sprint `sp-plan`,
*when* the operator runs `sprint abandon sp-plan --note "  " --as operator` and then
`sprint abandon sp-plan --note "the release is pulled" --as operator`,
*then* the first is refused with `abandon note is required` and the row is untouched; the second
returns status `abandoned` with `ends_at` stamped and the note on the `sprint_abandoned` payload;
*and when* the operator then tries to start it, *then* that is refused with `sprint sp-plan is
abandoned; open a new sprint instead of starting this one`.

### A7 — a v26 board (SPRINT-36, SPRINT-37, SPRINT-39)

*Given* a board file built at the real v26 ladder carrying a live pre-sprint task row,
*when* the compiled binary opens it and then runs the full lifecycle — `sprint new … --as
operator`, `sprint plan … --as operator`, `sprint start sp-… --as operator`, a bound
`deploy start … --sprint sp-… --as operator` and its verification `deploy finish d-… --token
TOKEN --result succeeded --phase verification --receipt "observed running tier"
--served-version X.Y.Z --as operator`, then `sprint close sp-… --deployment d-… --as operator`,
*then* the migration preserves the legacy row's title, the sprint reaches `closed`,
`audit verify` reports both the registry and the board healthy, and the search index holds one
`sprint` document per sprint with its cached document identity and embeddings intact.

### A8 — the projections (SPRINT-29, SPRINT-30, SPRINT-31, SPRINT-32, SPRINT-33)

*Given* a board with `sp-current` current at `1.2.3` holding one open row, one row the caller may
not read, and a closed sprint with its served proof,
*when* the operator reads `kb dash`, `kb ctx` for an attached and an unattached task, and the
three sprint pages in a browser,
*then* `dash`'s `currentSprint` names `1.2.3` with `open` `1`, `done` `0`, the goal headline and
a positive `daysRemaining`; the attached task's context names `sp-current`, `1.2.3` and the goal
while the unattached task's context has no `sprint` field and no `## Sprint` section; the board
page marks the current card and lists the closed one beneath it; the detail page shows the served
version and commit for the closed sprint; and neither the counts nor the listed rows include the
row the caller may not read.

### Categories deliberately not exercised here, and why

- **Authentication and session handling.** Not applicable: kanban implements none, and sprint
  authorization is the store's board-scoped check (`rust/store.rs:7707`), exercised by the
  visibility cases in A8.
- **Concurrency.** Exercised where it is decidable: the one-current-sprint rule is taken under
  `BEGIN IMMEDIATE` and independently held by a partial unique index, and SPRINT-07 asserts the
  same refusal from both paths. A wall-clock race between two processes is not simulated, because
  the write lock makes the second caller's read authoritative rather than lucky.
- **Idempotency.** Every verb is a state transition with a named refusal for the state it is
  already in (SPRINT-11, SPRINT-12), so a repeat is refused rather than silently repeated;
  A2 and A6 exercise that.
- **Load, capacity and failover.** Deliberately unexercised: this slice makes no such commitment
  and §6 invents none.

## 5. Contracts and data

- **Interface version or schema:** four interfaces. (1) The CLI grammar — seven `sprint`
  subcommands and the six sprint flags on `task add`, `task update`, `claim`, `handoff accept`,
  `deploy start` and `deploy finish` (`rust/lib.rs:223`-`rust/lib.rs:231`, `rust/lib.rs:133`,
  `rust/lib.rs:151`, `rust/lib.rs:158`, `rust/lib.rs:182`, `rust/lib.rs:105`,
  `rust/lib.rs:118`); `schema --json` and the MCP tools follow from that table (ADR-010) with no
  hand-written tool, and `SPRINT_FIELDS` (`rust/lib.rs:4164`) joins the field-list-drift guard.
  (2) The board schema — `BOARD_V27` and `BOARD_V28`, at `BOARD_SCHEMA_VERSION` 29 at this
  baseline (`rust/db.rs:2294`; V29 is the tag-rename change and is unrelated to sprints).
  (3) The event schema — six kinds with the payload keys named in SPRINT-34, additive to the
  ADR-029 chain. (4) The three read-only HTML routes and their `data-sprint-*` attribute contract
  (SPRINT-31).
- **Data invariants:** a sprint id is `sp-` plus a suffix of at most 64 ASCII characters from
  letters, digits, dot, underscore and hyphen. `status` is one of exactly `planned`, `current`,
  `closed`, `abandoned`, and at most one row on a board is `current` — held by the store under
  the write lock and independently by `CREATE UNIQUE INDEX one_current_sprint ON sprints(status)
  WHERE status='current'`. `target_version` is `X.Y.Z(-suffix)?`. `scheduled_end >=
  scheduled_start`, enforced in the schema as well as the store. `task_sprints` has `task_id` as
  its primary key, so a task is in at most one sprint, and it records `attached_by` and
  `attached_at` for every attachment; it references `tasks(id)` `ON DELETE CASCADE` and
  `sprints(id)`. `starts_at` is `0` until started; `ends_at` and `closed_by_deployment` are set
  only by `close`/`abandon`. The `deployments` ALTERs add `sprint_id` (referencing `sprints`),
  `target_version` and `served_version`; a bound succeeded verification attempt has all three set
  and its `served_version` equals its `target_version`. Search rows: one `search_documents` row
  per sprint at `source_kind='sprint'`, unique on `(source_kind, source_id)`, refreshed by the
  source triggers and cited as `kanban://BOARD/sprint/ID`.
- **Migration:** forward-only and additive, appended to `BOARD_MIGRATIONS`. `BOARD_V27` creates
  `sprints`, `one_current_sprint` and `task_sprints` and adds the three deployment columns;
  `BOARD_V28` widens the search document's `source_kind` CHECK to include `sprint` — which SQLite
  cannot do in place, so the document table is rebuilt with all fifteen columns copied explicitly
  and the external-content FTS table rebuilt from the preserved rows — and backfills existing
  sprints. There is no down migration: production migrations fail closed, and an unexpected
  object is incompatible state, not evidence the migration already ran.
- **Compatibility:** a board that has no sprint row behaves exactly as it did at schema 26 —
  every claim, handoff, dashboard and context byte is unchanged (SPRINT-22, SPRINT-19). An older
  binary reading a migrated board is out of contract, as it is for every board migration. The
  `tasks` table is not rebuilt and grows no column, so nothing that reads tasks changes shape.
- **Ownership:** the board's SQLite file owns sprint rows, attachments and events, read and
  written only through `Store`. The deployment attempt and its served-version observation are the
  deployment ledger's (ADR-030); this slice reads them and never writes a receipt. The registry
  owns sprint-scoped rule selectors and validates them against the board's sprint rows
  (`rust/registry.rs:328`-`rust/registry.rs:333`). Sprint rows carry no tags and are never
  automatically archived.

## 6. Quality and security

- **Reliability:** every verb is one transaction: the row update, its attachment moves and its
  events commit together or not at all (SPRINT-27, SPRINT-15), so a refused command leaves the
  board exactly as it found it. The one-current-sprint rule is enforced twice — under the write
  lock and by a partial unique index — so it cannot fail open from a bug in either. No
  availability or uptime commitment is made or implied.
- **Accessibility:** the three sprint pages are the ADR-046 design system's, preserved by
  citation: the same row, pill, table and drawer rules `docs/specs/web-ui.md` measures, which is
  what SPRINT-32 loads at 390, 820 and 1280 px. This slice claims no conformance of its own
  beyond what those requirements measure.
- **Privacy:** rows a caller may not read contribute neither content nor aggregate existence to
  any sprint projection (SPRINT-33). A refusal names ids and versions — never a restricted row's
  title, tags or body. The served pages reach no third party, as ADR-016 requires of every page.
- **Security:** authorization is the store's and is not re-implemented here — a sprint row
  carries no tags, so the check is the board-scope write check every other board write takes
  (`rust/store.rs:7707`), and each moved or read task row is additionally checked against its own
  tags (`rust/store.rs:7799`-`rust/store.rs:7800` on plan,
  `rust/store.rs:5030`-`rust/store.rs:5031` on attachment,
  `rust/store.rs:7964`-`rust/store.rs:7967` on close). No
  sprint projection ever carries a lease token, a capability token or a credential: the sprint row
  has no such field, the deployment proof is projected as its id, served version and served commit
  only, and the attached rows are projected without one (SPRINT-33). The sprint verbs add no HTTP
  write surface — the served pages are reads (SPRINT-31). The close gate never parses text: it
  compares typed columns, so a receipt cannot talk its way past it (SPRINT-24).
- **Operability:** `sprint list`/`show` and the three pages answer "which boundary is live, what
  is in it, and what proved it" without opening a database by hand. `sprint show` resolves the
  closing deployment for the reader, so the proof is one command away from the sprint. Nothing
  archives or advances a sprint because time passed; every transition is an audited operator act.
- **Performance:** one bound, and it is a refusal rather than a budget: `sprint list` is capped at
  the ADR-037 default of 100 rows and refuses a page it would exceed instead of truncating
  silently (SPRINT-13). The served board page reads every sprint on the board with two counting
  queries per card (`rust/serve.rs:2709`, `rust/serve.rs:2748`); no latency, throughput or payload
  number is measured or committed here. §7 OQ-2 carries the question that would otherwise be
  answered with a guess (ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | What is actually served today, and does it carry this slice? As a dated fact rather than a requirement: on 2026-09-19 `hax` and `hig` serve release `461b1c1` at board schema 28 (kanban 0.3.0), which does carry the sprint slice — the sprint release shipped 2026-09-18 — while release `d15822d` at board schema 29 is in its gate today. `docs/PRD.md:431` still names commit `8e771ee` at schema 28 for the same claim. Nothing in §3 depends on this; the question is only which sentence the estate's served-state record should carry. | George | open | the served-state record in `docs/PRD.md` "Current delivery status" (not this specification's gate) |
| OQ-2 | Is there a sprint-count or listing-latency target for `/sprints` across every registered board, which renders one card per sprint per board with two counting queries each? Deliberately not invented here (ADR-047 §9); the only bound that exists today is ADR-037's 100-row CLI cap. | George | open | any performance commitment on the sprint pages |
| OQ-3 | ADR-045 §2 lists `sprint list [--status …] [--all]` and ADR-045's Consequences promise the seven verbs; the shipped surface also carries `--limit`, `--fields`/`--no-body` and `sprint new --id`. Are those part of the contract this specification pins, or incidental? Pinned here as contract (SPRINT-01, SPRINT-13) because the drift guard already tests them; the question is whether ADR-045 §2 should be amended to say so. | George | open | an ADR-045 amendment; nothing in §3 |
| OQ-4 | Should the usage table publish `--sprint`/`--any-sprint` on the `claim --candidates` line? The flags are honoured there today — the candidates path builds the same `ClaimOptions` (`rust/lib.rs:6490`) and resolves the boundary through the same resolver (`rust/store.rs:5195`-`rust/store.rs:5197`) — but `rust/lib.rs:162`-`rust/lib.rs:164` does not name them, so a reader of `kanban --help` cannot tell that the read-only inspection can be pointed at another boundary. ADR-045 §3 speaks of "`claim --candidates` and `claim --next`" together. | George | open | the usage table's accuracy; no requirement above depends on it |

## 8. Verification

The planned evidence for every mandatory requirement. This table is the draft of the matrix rows;
`docs/testing/compiled-rust-e2e-matrix.md` is the trace of record and carries the same rows
verbatim. `Layer` is named precisely and is never `e2e` for an in-process test.

`Test name` is an **existing** test function at this baseline. The `process` and `chrome` names
were enumerated with `cargo test --locked --test e2e -- --list`; the `unit` names were verified as
`fn <name>(` in `rust/store.rs`, `rust/serve.rs` and `rust/db.rs`, because the lib test target
does not compile in this documentation worktree. A `none` row says `no e2e coverage` plainly and
names what would have to be written.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `SPRINT-01` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | the compiled binary refuses `--id not-a-sprint` with the `must start with sp-` sentence and writes no row |
| `SPRINT-02` | MUST | unit | `a_version_must_be_semver_shaped_before_a_sprint_exists` | `rust/store.rs` `mod tests`: the validator over seven bad shapes, then `create_sprint` refusing `v1` with the `X.Y.Z` sentence quoting the value |
| `SPRINT-03` | MUST | process | `every_enum_argument_refusal_names_the_whole_set` | the `sprint-list-status` row of `ENUM_ARGUMENTS`; the same test proves `schema --json` publishes the set it names |
| `SPRINT-04` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (run `sprint new` with a negative `--start`, with an `--end` before `--start`, and with a non-integer `--start`, asserting the `sprint schedule requires non-negative --start and --end at or after --start` and `--start must be an epoch-millisecond integer` refusals). The schema `CHECK` is exercised only incidentally by the ladder test today |
| `SPRINT-05` | MUST | unit | `starting_a_second_current_sprint_is_refused_naming_the_holder` | asserts `starts_at > 0` only after a successful start; `a_planned_sprint_cannot_close_and_abandon_needs_a_note` asserts `ends_at` is set by `abandon`, and `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` asserts `closedByDeployment` after a close |
| `SPRINT-06` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (point `claim`, `task move` and `handoff create` at an `sp-` id and assert each is refused as an unknown task, leaving the sprint row untouched). The property is structural today — `sprints` has no claim, lease or gate column and no `sprint` subcommand takes a lease |
| `SPRINT-07` | MUST | unit | `starting_a_second_current_sprint_is_refused_naming_the_holder` | asserts the exact holder sentence, that the refused row stays `planned`, and that no event was appended; `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context` asserts the same refusal over the compiled binary |
| `SPRINT-08` | MUST | process | `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context` | asserts the `requires --candidate, --parent-epic, existing explicit scope, or --empty-scope` refusal; `sprint_plan_records_only_real_scope_moves_with_previous_assignment` holds the `--empty-scope` exclusivity |
| `SPRINT-09` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (run `sprint plan` with no `--body`/`--body-file`, and with a blank one, asserting the `the goal and success criteria are the plan` and `sprint body is required` refusals). Every `sprint plan` case in the suite passes a body today |
| `SPRINT-10` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | asserts the `no recorded goal and criteria` refusal on an unplanned start; the `has not been planned` and `no deliberate scope` refusals are reached only through the store today |
| `SPRINT-11` | MUST | unit | `a_planned_sprint_cannot_close_and_abandon_needs_a_note` | the `planned, not current` close refusal and the `is abandoned; open a new sprint instead of starting this one` start refusal; `starting_a_second_current_sprint_is_refused_naming_the_holder` holds the `current and cannot be replanned` refusal with the body and event count unchanged |
| `SPRINT-12` | MUST | unit | `a_planned_sprint_cannot_close_and_abandon_needs_a_note` | `abandon note is required`, then status `abandoned` with `ends_at` set and the note on the `sprint_abandoned` payload. No compiled-binary case runs `sprint abandon` |
| `SPRINT-13` | MUST | process | `every_capped_listing_refuses_a_default_it_would_exceed_and_answers_one_it_meets` | the `sprints` row of `CAPPED_LISTINGS` at default 100; the default hiding of `closed`/`abandoned` rows and the newest-first order are asserted by no test today |
| `SPRINT-14` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | reads the carry destination back with `sprint show --json` and finds the carried row; `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` reads the destination's rows and their statuses the same way |
| `SPRINT-15` | MUST | process | `sprint_parent_epic_scope_preserves_explicit_descendants_and_audits_detach` | the epic's subtree attaches, a descendant already in another sprint is preserved, and `--clear-sprint` emits one `task_sprint_changed` with a null `newSprintID`; `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` asserts the whole-command rollback when `--sprint` names a missing sprint |
| `SPRINT-16` | MUST | unit | `attaching_an_epic_carries_its_subtree_skipping_other_sprints_rows` | asserts the `abandoned; attach the rows to a planned or current sprint instead` refusal; `sprint_scoped_rule_transfer_requires_destination_sprint_and_round_trips` reaches the same gate from the carry-over destination side |
| `SPRINT-17` | MUST | unit | `claims_are_scoped_to_the_current_sprint_and_overrides_are_recorded` | the default pool is the current sprint's rows only, `--any-sprint` restores all three, a named sprint sees its own, and unknown and abandoned names are refused by their exact sentences |
| `SPRINT-18` | MUST | process | `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides` | asserts `not in sprint sp-current-boundary` and that the override flags then succeed; `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` asserts the same refusal names `--any-sprint` |
| `SPRINT-19` | MUST | process | `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides` | reads `sprintOverride` off the `task_claimed` events in `kb ev`; `claims_are_scoped_to_the_current_sprint_and_overrides_are_recorded` asserts a default-scoped claim grows no payload key |
| `SPRINT-20` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | a handoff on an out-of-boundary row is refused at acceptance naming `--any-sprint`, then accepted with it, and the accepted claim names the task |
| `SPRINT-21` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (pass `--sprint` and `--any-sprint` together on each of `claim --candidates`, `claim` and `handoff accept`, asserting the `--sprint and --any-sprint both answer which sprint boundary this claim crosses` refusal, `rust/lib.rs:2488`). No test exercises any of the three call sites today |
| `SPRINT-22` | MUST | unit | `a_board_without_a_current_sprint_claims_exactly_as_before` | a merely `planned` sprint leaves the pool and the claim payload untouched; `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides` runs the same check over a second compiled-binary board with no sprint |
| `SPRINT-23` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | asserts the `requires --deployment` refusal |
| `SPRINT-24` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | asserts the `failed (verification)` refusal and the non-verification phase refusal verbatim; `a_sprint_closes_only_on_a_succeeded_verification_deployment` holds the same gate at `unit` |
| `SPRINT-25` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | asserts `bound to this sprint and served version 5.0.0` for an attempt bound elsewhere |
| `SPRINT-26` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | the start stores `sprintID` and `targetVersion`, a `9.9.9` finish is refused naming `target version 1.2.3`, and the matching finish succeeds |
| `SPRINT-27` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | the `--carry-to` refusal, the `carry-over note` refusal, the destination holding the carried row at its own status, and the carry and close events with their payloads |
| `SPRINT-28` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (close a sprint whose every attached row is `done`/`cancelled` while passing `--carry-to`/`--carry-note`, and a close naming itself as the destination, asserting `has no unfinished rows to carry over` and `carry-over destination must be a different sprint`). No test closes a fully finished sprint with carry flags today |
| `SPRINT-29` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | reads `currentSprint` off `dashboard --json`: target version, open, done, goal and a positive `daysRemaining`; `a_context_packet_carries_the_tasks_sprint_and_the_dash_counts_it` holds the same projection at `unit` |
| `SPRINT-30` | MUST | process | `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context` | the attached task's JSON and text context, and the unattached task's absent field and absent `## Sprint` section; `context_packet_task_and_sprint_share_one_read_snapshot` holds the one-snapshot rule |
| `SPRINT-31` | MUST | unit | `serve_render_fixture_child_process` | reads the served bytes of `/sprints`, `/sprints/{project}` and `/sprint/{project}/{id}`: the board section, the current card's `data-sprint-*` values, the history cards' states, the archived marker and the empty-board sentence |
| `SPRINT-32` | MUST | chrome | `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records` | reaches `/sprints` from the drawer and `/sprints/{project}` from a board link at phone width; `no_route_overflows_sideways_at_three_widths_in_real_chrome` loads all three sprint routes, including a seeded `/sprint/{project}/{id}`, at 390, 820 and 1280 px |
| `SPRINT-33` | MUST | unit | `sprint_projection_counts_and_proof_obey_tag_visibility` | `rust/serve.rs` `mod tests`: a restricted row contributes neither content nor count to the served card or the detail page; `managed_sprint_projections_hide_restricted_rows_counts_and_event_ids` holds the same rule over the store's own projections |
| `SPRINT-34` | MUST | process | `sprint_v26_board_migrates_then_completes_a_proof_gated_lifecycle` | runs the whole lifecycle and asserts `audit verify` healthy for the registry and the board; `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` asserts the close and carry payload keys, and `sprint_parent_epic_scope_preserves_explicit_descendants_and_audits_detach` the attachment payloads |
| `SPRINT-35` | MUST | unit | `sprint_plan_records_only_real_scope_moves_with_previous_assignment` | one event per row that actually moved, each carrying its `oldSprintID` |
| `SPRINT-36` | MUST | unit | `board_schema_v27_migrates_a_real_v26_board_and_preserves_rows` | `rust/db.rs` `mod tests`; `v27_refuses_unexpected_objects_and_rolls_back_without_partial_schema` holds the fail-closed half |
| `SPRINT-37` | MUST | process | `sprint_v26_board_migrates_then_completes_a_proof_gated_lifecycle` | the compiled binary opens a real v26 board, keeps the legacy row readable, and closes a sprint on a bound verification attempt |
| `SPRINT-38` | MUST | process | `sprint_search_is_board_scoped_fresh_rebuildable_and_exactly_cited` | the `kanban://BOARD/sprint/ID` citation, freshness across lifecycle writes, board scoping and rebuild; `v27_board_migrates_sprint_search_with_citation_cache_identity_and_index_health` holds the same over a board migrated from V27 |
| `SPRINT-39` | MUST | unit | `board_schema_v28_backfills_sprints_without_rekeying_or_erasing_cached_documents` | `rust/db.rs` `mod tests`: `seq`, `source_hash` and cached embeddings survive the document-table rebuild and existing sprints are backfilled |

**Counts.** 39 requirements, all `MUST`, no `SHOULD` and no `MAY`. By layer: 20 `process`, 13
`unit`, 1 `chrome`, 5 `none`. By group: the sprint row and its identity 6
(`SPRINT-01`..`SPRINT-06`), the seven verbs 10 (`SPRINT-07`..`SPRINT-16`), sprint-scoped claims 6
(`SPRINT-17`..`SPRINT-22`), done means served 6 (`SPRINT-23`..`SPRINT-28`), projections 5
(`SPRINT-29`..`SPRINT-33`), events 2 (`SPRINT-34`..`SPRINT-35`), schema and migration 4
(`SPRINT-36`..`SPRINT-39`).

**Rows with no evidence yet: 5.** `SPRINT-04` (the schedule-window refusal), `SPRINT-06` (a
sprint id is not claimable), `SPRINT-09` (`sprint plan` without a body), `SPRINT-21` (`--sprint`
with `--any-sprint`) and `SPRINT-28` (carry flags with nothing to carry). Each is implemented
behaviour with a refusal sentence quoted in §3 from the line that produces it; what is missing is
a test that fails when the sentence changes. They are not a rollout blocker for a shipped slice,
but they are the five cases a follow-up row under epic `e-ee95dbd5` should write.

**Coverage the table does not claim.** `sprint abandon` and `sprint list` are exercised over the
compiled binary only incidentally — `abandon` through store-level unit tests (`SPRINT-12`) and
`list` through the listing-cap and enum-refusal sweeps (`SPRINT-03`, `SPRINT-13`). No test
asserts that the default `sprint list` hides `closed` and `abandoned` rows or that it is ordered
newest first.

## 9. Change log

- `2026-09-19` — created at `Draft — gate requested`, against commit `d15822d`. No requirement
  supersedes another: this is the slice's first specification, written over behaviour ADR-045
  records as implemented, and it specifies only what that ADR decided (ADR-047 §6 — nothing is
  specified retroactively beyond the slice being touched).
- `2026-09-19` — `docs/PRD.md:209` corrected in the same change: its `### Sprint release
  boundaries` bullet read "Offer plan, start, close, and abandon over that row plus read-only
  list and show", which omits the creation verb the shipped surface carries and ADR-045 §2
  counts among its seven (`rust/lib.rs:223`). The sentence now names `new`. No other sentence in
  either PRD sprint section disagrees with implemented behaviour.
- `2026-09-19` — findings recorded rather than papered over: ADR-045 §1 says "V28 is now the
  current board schema", but `BOARD_SCHEMA_VERSION` is `29` at this baseline
  (`rust/db.rs:2294`) — V29 is the unrelated tag-rename change, and the sprint schema is
  unaffected; ADR-045 §3 names the claim event `task_claim`, while the kind actually written is
  `task_claimed` (`rust/store.rs:5155`); ADR-045 §5 says `kb ctx` carries "sprint id, target
  version and goal headline", while the shipped packet also carries the sprint's `status` and
  `title` (`rust/store.rs:7091`-`rust/store.rs:7097`). §7 OQ-3 and OQ-4 carry the two surface
  questions those readings raise. None of the three changes an obligation above; each is an
  ADR-045 wording amendment for its owner.
