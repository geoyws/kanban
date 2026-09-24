# Specification: a decision card checks reusable system understanding before the decision (slice ACC)

## 1. Identity and baseline

- **Slice ID:** `ACC`. Requirement IDs are `ACC-01` .. `ACC-21`, stable across wording
  refinements; numbering is by creation and grouping is by topic.
- **Baseline:** `2026-09-21` at commit `04a66b1` on branch
  `docs/t-6a3dd1a6-acc-spec`.
- **Status:** `SPEC-READY` on 2026-09-21. An independent `/quality spec` review found five
  defects; all were closed, and George resolved its sole product blocker as **lock** on decision
  card `a-db9de88b`. Specification readiness authorises neither implementation, rollout nor release.
- **Status (delta 2026-09-23):** `SPEC-READY` for ACC-18/ACC-19. An independent reviewer
  verified the delta against the SDD §1 exit criteria (findings F1–F4 raised and closed, §8
  single-row-per-requirement restored). Specification readiness authorises neither
  implementation, rollout nor release.
- **Status (delta 2026-09-24):** owner-authorised scope change on board task `t-1aa9f553`
  (George, 2026-09-24, chose "Ledger + skills"): ACC-06, ACC-15 and ACC-17 superseded in place,
  ACC-20 and ACC-21 appended. Drafted with the implementation on branch
  `wt/t-1aa9f553-ledger` from base `f7cc09f`.
- **Status (delta review 2026-09-29):** `SPEC-READY` for the 2026-09-24 delta (independent review
  by the `t-0382c937` lane against the SDD §1 exit criteria, no findings). Baseline, owner and
  source explicit; IDs stable with strengths; behaviours observable with permissions
  (ACC-21 actor rule) and named refusals; A19 covers ACC-06/20/21 happy, invalid and
  unauthorized paths beside A6; CLI grammar and JSON receipt keys enumerated; verification
  mapped to existing tests (`attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits`,
  `answered_check_locks_definition_and_a_later_resolve_reuses_it`); no open questions, the
  owner decided every scope point. Specification readiness authorises neither implementation,
  rollout nor release.
- **Owner (product scope):** George.
- **Decider (wording of this document):** George.
- **Sources:**
  - `e-5c8f7735` — the native ACC ledger block, reads, first-draft retry behaviour and
    rollout boundary; George promoted it on 2026-09-18 (note 103).
  - `e-bef5dd2a` — a check tests reusable codebase/system understanding, carries `about`,
    refuses diagnosis-shaped content and is authored only by the raiser.
  - `t-cbaff421` — field set, all-or-none validation, persistence and schema boundary.
  - `t-d7e6af5f` — resolve fields and the later one-answer/no-retry behaviour that expressly
    says it supersedes the epic's first draft.
  - `t-83d48345` — mounted web journey, server-side redaction and the check-answer POST.
  - `t-94076221` — native reader/skill cutover and one-shot body-block migration.
  - `t-b5bc413f` — exact diagnosis markers, subject shapes and refusal data rules.
  - `t-1227a592` — raiser-only authoring with no `geoyws` exception.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — specification
    format, stable IDs, trace and readiness gate.
  - George, 2026-09-20 (walkthrough note 255) — approved adding ACC to the SDD rollout.
  - `a-db9de88b` on `t-6a3dd1a6` — George resolved OQ-1 as **lock** on 2026-09-21.
  - George, 2026-09-21 (scope decision recorded on `t-cbaff421`) — every later pre-answer
    read redacts answer and explanation, including the raiser's own show; the successful raise
    receipt alone may echo the just-authored definition.
  - George, 2026-09-24 (scope decision on `t-1aa9f553`, "Ledger + skills") — `/kb-att` clears
    attention rows with no check quiz in the way; checks move to a separate `/kb-acc` run when he
    has time, and every answer there says right or wrong, gives the correct choice and the
    explanation, for a pass as much as a miss ("ACC doesn't even correct me atm if i was wrong
    or explain why i was right"). The ledger therefore settles a checked row without its answer
    and accepts the one answer later, open or resolved.
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries one row per
  requirement and names implementation evidence only after it lands.

## 2. Purpose and scope

**Intended outcome.** Before George makes a durable decision, the decision card asks one
raiser-authored question that demonstrates the reusable system fact the decision depends on,
records the answer as data and never exposes its answer key to the browser in advance.

**Users / actors.**

- **Raiser** — authors and may update a native check while holding the relevant code,
  component, host or tier context.
- **George / decision maker** — answers the check through the web card, the CLI
  (`attention resolve --check-answered` or `attention check`) or `/kb-acc`, and makes the
  existing decision, before or after the answer.
- **Clerk / other lane / MCP caller** — reads the permitted redacted projection but cannot
  synthesize, add or change a check.
- **Operator** — migrates legacy body-block ACCs and reads aggregate check results by subject.

**In scope.** The native check block on `attention raise` and `attention update`;
`attention resolve --check-answered`; `attention check`; its Store invariants and persisted
result; list, show, MCP and digest projections; `POST /attention/{project}/{id}/check`; the
mounted decision card; legacy `ACC:` body-block migration; the `/kb`, `/kb-att` and `/kb-acc`
native-field cutover; `attention list --check-report` and one aggregate summary block on
`/decided`.

**Boundaries.** Existing board/tag tenancy, actor authentication, attention status transitions,
decision outcomes, choice key/label bounds, card hotkeys and Undo remain owned by their current
contracts. ACC inherits those laws rather than restating or weakening them.

**Rollout boundary.** ACC is the one George-approved addition to the ADR-047 rollout. It does not
make unrelated slices retrospective specifications. Implementation may begin only after this
specification reaches `SPEC-READY`; release and deployment remain separate later gates.

**Non-goals.**
- ACC does not diagnose an incident, test recall of arbitrary row prose, ask for row status, next
  action, owner or decision choice, merely restate an answer, recommend a decision, select an
  outcome or replace the decision card. These describe the intended reusable-system teaching
  purpose; only ACC-02's subject shapes and ACC-04's short marker list are Store refusals.
- ACC does not define a performance, latency, availability or retention target.
- ACC does not duplicate existing board/tag authorization rules.
- The miss-rate report is never a leaderboard and never scores raisers: its groups key on
  `about` alone and carry no actor, raiser, answer-key, explanation or choice-label column
  (ACC-18, ACC-19).

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the planned evidence layer: `unit`, `http`, `chrome`
or `process`.

### Native check contract

**ACC-01 — Accept only a complete bounded check block.**
Strength: MUST · Layer: process · Source: `e-5c8f7735` §Ledger; `t-cbaff421` FIELDS/REFUSALS.
`attention raise` and `attention update` accept either none of the five native check inputs or all
of them: one question of at most 160 characters ending in `?`; two through four choices using the
existing decision-choice key and label bounds; exactly one answer naming a declared check choice;
one explanation of at most 400 characters; and one `about` subject. A partial block, length
overrun or answer naming an undeclared key is refused before the write, names the offending field,
and leaves the row and event history unchanged.

**ACC-02 — Make the explanation and subject identify what is being taught.**
Strength: MUST · Layer: unit · Source: `e-bef5dd2a` §What the kanban team owns 1;
`t-b5bc413f` REFUSE.
The explanation names the relevant component, file, host or tier. `about` is required and matches
one of exactly these agreed subject shapes: a path containing `/` or a file extension; a sigil
token `@@host` or `@_tier`; or an `UPPER_SNAKE` flag or `--flag` name. Data rule, verbatim from
`t-b5bc413f`: **“`about` not matching one of the three subject shapes (a path containing `/` or a
file extension; a sigil token `@@host`/`@_tier`; an UPPER_SNAKE flag or `--flag` name) is refused
naming the three shapes.”** The refusal happens before the write and leaves prior data unchanged.

**ACC-03 — Keep check choices non-decisional.**
Strength: MUST · Layer: unit · Source: `e-5c8f7735` §Ledger; `t-cbaff421` FIELDS.
A check has no recommendation, its choices have no outcome, and neither its answer nor result can
reach or change `decision.outcome`. Any request that tries to attach recommendation or outcome
semantics to a check is refused before the write.

### Meaning and authoring


**ACC-04 — Refuse the agreed diagnosis shape as a data rule.**
Strength: MUST · Layer: unit · Source: `t-b5bc413f` REFUSE.
A question, any choice label or explanation is refused before the write when it contains a typed
row ID (`a-`, `t-`, `s-`, `e-` or `sp-` plus eight hexadecimal characters), an absolute
`YYYY-MM-DD` date, or the case-insensitive word-bounded phrase `today`, `this row`, `the sweep` or
`measured`. The refusal names the offending token. Data rule, verbatim from `t-b5bc413f`:
**“The refusal prints the offending token and the rule: a check teaches how the system works; it
never reads this row's finding back.”** No broader classifier or unapproved marker list is implied.

**ACC-05 — Let only the raiser author or change a check.**
Strength: MUST · Layer: process · Source: `e-bef5dd2a` §What the kanban team owns 4;
`t-1227a592` RULE; George's **lock** on `a-db9de88b`.
The Store accepts check-definition inputs on raise and on update only when the actor is the row's
`raisedBy`. An update carrying any check-definition input from another lane, a clerk or `geoyws`
is refused naming the raiser and writes nothing. Resolve accepts only an answer input and can
never add or change the definition. MCP and web calls inherit the same Store rule. Once an answer
is recorded while the row remains open, every check-definition edit is refused; its definition,
answer/result and unlocked decision choices remain stable until normal resolution. A resolved
row's card is immutable. Reopen clears its current decision and check result, returns it to open
with decision choices locked behind a new answer, and restores raiser update.

### Answer and resolution

**ACC-06 — Settle a checked row with or without its answer.**
Strength: MUST · Layer: process · Source: `e-5c8f7735` §Ledger; `t-d7e6af5f`; George,
2026-09-24 on `t-1aa9f553`.
`attention resolve ... --check-answered KEY` is optional for a row carrying an unanswered check.
Supplied, it records the one answer (ACC-07) before the row resolves. Omitted, the row resolves
exactly as a row without a check does: no check result is written, no `ACC:` echo is added to the
resolution, and the check stays pending — redacted on every read (ACC-13) and answerable later
(ACC-20). If the one answer is already recorded, a later web or CLI resolve finds it, asks
nothing twice and requires no second `--check-answered`. Supplying the flag for a row with no
check is refused by name. A row with no check and no answer input resolves exactly as before ACC.

*Superseded 2026-09-24.* The wording this replaces read: "`attention resolve ...
--check-answered KEY` is required for a row carrying an unanswered check. Omitting it refuses
resolution and prints the check question; the row stays open and no decision, check result or
event is written." That gate put a quiz in front of every decision George clears through
`/kb-att`. He chose, on 2026-09-24 (`t-1aa9f553`, "Ledger + skills"), to clear decisions
without it and answer checks separately through `/kb-acc`; the answer is still recorded exactly
once, now possibly after resolution (ACC-20).

**ACC-07 — Validate and persist the submitted answer as data.**
Strength: MUST · Layer: process · Source: `t-d7e6af5f`; `e-bef5dd2a` §What the kanban team owns 3.
A declared choice key records `check.answered`, `check.correct = (KEY == answer)` and
`check.answeredAt`; the human note echo is `ACC: pass` or `ACC: miss on <key>` and agrees with the
stored fields. A submitted key not declared by the check is refused before any resolution or
result write. Reads can distinguish `pass`, `miss:<key>` and `none` by subject without parsing
note text. No unapproved event name or field name is specified here.

**ACC-08 — Record exactly one answer and never retry the check.**
Strength: MUST · Layer: process · Source: `t-d7e6af5f` ALIGN WITH THE SKILL/Acceptance;
`t-83d48345` BEHAVIOUR; supersedes `e-5c8f7735`'s first-draft retry wording.
Each check accepts one answer. A wrong declared key records `correct=false` and resolves the row
when supplied to resolve; in the web flow it records immediately and permits the decision click.
A right key records `correct=true` and does the same. There is no retry loop, no `attempts`
counter, no re-hiding after a miss and no second quiz gate. Answer writes serialize: the first
accepted answer is immutable, and every later identical or different submission is refused
(web conflict; CLI refusal) without replacing the answer, result, decision or event history.

### Web card and HTTP boundary

**ACC-09 — Put the check before an inert decision.**
Strength: MUST · Layer: chrome · Source: `e-5c8f7735` §Web; `t-83d48345` BEHAVIOUR.
The current card first shows `about` as its subject chip, the check question and two through four
answer controls in authored order. Decision choices are present but inert and cannot be submitted
until the server has recorded a check answer. Once recorded, the existing decision controls
become operable; a miss shows the explanation above them and does not re-hide or offer retry.

**ACC-10 — Keep the answer key and pre-answer explanation out of browser bytes.**
Strength: MUST · Layer: chrome · Source: `e-5c8f7735` §Web; `e-bef5dd2a` §What the kanban team
owns 5; `t-83d48345` BEHAVIOUR.
Before an answer is recorded, neither the answer key nor the explanation occurs in HTML, JSON,
JavaScript, the embedded bundle, DOM attributes, hidden nodes or any response available to that
browser. The server-side projection omits both; client-side hiding is insufficient. The public
refusal for a wrong or invalid answer does not reveal or allow derivation of the key. After an
answer is recorded, the response may reveal the explanation and answer key.

**ACC-11 — Record browser answers through the shared Store operation.**
Strength: MUST · Layer: http · Source: `t-83d48345` ENDPOINT.
`POST /attention/{project}/{id}/check` carries one submitted choice key and invokes the same Store
answer operation as CLI `--check-answered`; it cannot implement a second answer law. The actor is
the existing trusted-edge actor, and existing same-origin, tenancy and tag authorization applies.
The response exposes only the post-answer projection allowed by ACC-10 and ACC-13. Concurrent
answer requests serialize; the loser observes the accepted first answer and returns the existing
conflict/refusal without mutation. The operation is added to the project's pinned OpenAPI contract;
request, successful/refused responses, authorization and redaction are described there without
inventing a new API version.

**ACC-12 — Preserve keyboard, Undo and accessible state after unlock.**
Strength: MUST · Layer: chrome · Source: `t-83d48345` BEHAVIOUR/LAYER; existing WEB contracts.
Before answer recording, digit activation operates only the check controls and cannot submit a
decision. After unlock, existing decision digit hotkeys, Skip, custom answer and Undo behave
unchanged. Check controls have programmatic names containing their authored labels, keyboard and
pointer activation are equivalent, focus moves to or announces the resulting explanation/decision
region, and lock/unlock and pass/miss feedback are not conveyed by colour alone.

### Reads, redaction and compatibility

**ACC-13 — Redact every pre-answer read projection.**
Strength: MUST · Layer: process · Source: `e-5c8f7735` §Reads; `t-cbaff421` PERSISTENCE and
George's 2026-09-21 scope decision; `t-94076221`.
For an authorized unanswered row, every show, list, MCP, digest and HTTP read projection carries
only the question, choices and `about`; explanation and answer are absent even when the reader is
the raiser. The successful `attention raise` write receipt may echo the complete definition to
the author in that same response, but no later read inherits that exception. The browser
projection obeys ACC-10. After the answer is recorded, permitted reads may carry explanation,
answer and the stored result fields from ACC-07. Redaction removes fields rather than substituting
a guessable placeholder.

**ACC-14 — Inherit tenancy and tag visibility without an ACC bypass.**
Strength: MUST · Layer: http/process · Source: rollout brief; existing Store authorization boundary.
Every ACC read and write is authorized as the containing attention row is authorized. A caller
who cannot read the row receives the existing non-enumerating unauthorized/not-found behaviour
and no check metadata; a caller who can read it gets the appropriate ACC-13 projection. ACC adds
no cross-board, cross-tenant, tag or actor exception, and denial is indistinguishable for checked,
unchecked, known and unknown rows.

- 2026-09-29 — task-attach writes inherit the task's tags (`t-d2fd604a` port of `9626f1f`,
  CLI paths only): `add_note`, `raise_attention` with `--task` and `post_sitrep` with `--task`
  are authorized against the named task's own tags at both read and write (`authorize_task_attach`
  in `rust/store.rs`), with an absent id answering the same `DeniedOrNotFound` error the denied
  path uses when enforcement is managed — so success no longer confirms a tag-denied row exists,
  and a refusal writes nothing. A lanewide sitrep and a taskless attention raise keep their
  existing board scope. Requirement wording is unchanged: this restores the
  authorize-as-the-containing-row rule the tag-checked writes already applied.

- 2026-09-25 — replacing a task's dependency list keeps the edges the caller
  cannot read (`update_task` in `rust/store.rs`): the new edges are still
  authorized one by one as task-attach writes, but the delete drops only the
  edges the caller could see and re-inserts the hidden ones beside the new
  list, so a writer on the row cannot silently remove a gate the owner set.
  Refusing the replacement instead would confirm a hidden edge exists, so the
  write succeeds — keep over refuse. The receipt and the dependency listing
  still withhold the kept edges while the gate keeps honouring every edge, so
  a kept gate still blocks claims. `--clear-dependencies` arrives as an empty
  list and keeps them the same way. Requirement wording is unchanged: this
  restores the authorize-as-the-containing-row rule for the replacement the
  whole-row delete had silently left.

- 2026-09-25 — every write that links a row to a task (`add_note`,
  `raise_attention` with `--task`, `post_sitrep` with `--task`, `checkpoint`,
  `create_handoff` with a task, the parent and dependency edges on task add
  and update, subscription subjects and relations, `start_deployment` with
  `--task`) is authorized against that task's own tags at both read and write
  (`authorize_task_attach` in `rust/store.rs`), with an absent id answering
  the same `DeniedOrNotFound` error the denied path uses when enforcement is
  managed — so success no longer confirms a tag-denied row exists, and a
  refusal writes nothing. A lanewide sitrep, a session handoff and a taskless
  deployment keep their existing board scope. Requirement wording is unchanged:
  this restores the authorize-as-the-containing-row rule the tag-checked
  writes (claim, sprint attachment, deployment finish) already applied.

- 2026-09-25 — paged listings whose rows are filtered per row
  (`events_with_bounds`, `events_since_filtered`, `deployments`) apply the
  caller's bound to the rows the caller may READ, not to the raw rows: under
  managed enforcement the SQL `LIMIT` ran before the tag test, so a denied
  row consumed a slot and the page came back short — `events --task
  t-visible --limit 1` answered `[]` while newer visible history waited,
  leaking hidden activity and its timing — and every caller's `+1`
  truncation probe with it. The scan now pages through the SQL in chunks
  until the bound is filled (`scan_visible` in `rust/store.rs`, following
  the attention listing's pattern), keeping the probe correct; unmanaged
  boards keep the SQL bound, byte-identical. `current_deployments` takes no
  bound and is unchanged. Requirement wording is unchanged: this restores
  the filter-before-truncate rule the attention, sitrep, handoff and
  resolved listings already apply (`t-b694bc83`).

- 2026-09-25 — every by-id attention operation (`show_attention`,
  `update_attention`, `resolve_attention`, `reopen_attention`,
  `answer_check_with_authorization`) and `Store::require_task` map an absent
  id to the same `DeniedOrNotFound` error the denied path uses when
  enforcement is managed (`absent_as_denied` in `rust/store.rs`), so the
  CLI/MCP named reads answer a denied id and an unknown one identically.
  Refusals past the guard keep their sentences; unmanaged boards
  keep the plain `not found` messages. Requirement wording is unchanged: this
  restores the indistinguishability for known and unknown rows it already
  states. Follow-up the same day: the same collapse reaches every task route
  that names a row — one authorized helper (`require_task_authorized`, with
  the still-active variant for mutations) serves the events history gate,
  move, remove, patch-metadata, update, story signoff and advance, sprint
  attach and detach, a named claim, and the `notes` and `checkpoints` reads;
  task-filtered attention, sitrep and handoff listings proceed for an unknown
  task exactly as they do for a denied one (`require_task_filter`); and an
  absent deployment answers the same denial as a denied subject's attempt.
  `add_note` still returns its just-written receipt without re-gating, so an
  authorized-at-board-scope write is never an error after commit. Writes that
  merely reference a task keep their behaviour.

**ACC-15 — Cut readers and skills over to native data.**
Strength: MUST · Layer: process · Source: `e-bef5dd2a` root cause; `t-94076221`.
`lane-att.sh`, `/kb-att` and `/kb-acc` read only the native check question, `about` and choices
before answer. `/kb-att` resolves attention rows without asking the check, which stays pending
(ACC-06). `/kb-acc` finds pending checks on open and resolved rows, asks each one and records the
answer through `attention check` (ACC-21), then shows the verdict, the correct choice and the
explanation from its receipt.
`/kb` authors the five native inputs. The legacy body-block reader and clerk synthesis are
removed: a row with no native check is shown with no check and reported as drafting debt, never
silently given a generated one.

*Superseded 2026-09-24.* The wording this replaces read: "`lane-att.sh` and `/kb-att` read only
the native check question, `about` and choices before answer, ask the check first and pass the
recorded answer to resolution." George moved the check out of `/kb-att` into a separate `/kb-acc`
run on 2026-09-24 (`t-1aa9f553`), and asked that every answer be corrected and explained.

**ACC-16 — Migrate legacy body blocks once without changing other rows.**
Strength: MUST · Layer: process · Source: `t-cbaff421` PERSISTENCE; `t-94076221` Migration.
The board schema advances at the ADR-045 version boundary and can persist the definition and
result fields. A one-shot migration converts an open row whose body starts with a valid legacy
`ACC:` block into the native fields once per board and records its operator receipt; rows without
that block are byte-for-byte unchanged. Reopening the migrated board does not duplicate the check
or result. Invalid legacy prose is reported and not partially migrated.

**ACC-17 — Preserve rows and clients that have no check.**
Strength: MUST · Layer: process · Source: `e-5c8f7735` §Ledger; `t-d7e6af5f`.
Existing and newly written rows without a check continue to list, show, render and resolve with
their pre-ACC observable behaviour. Their JSON/MCP projection omits the check rather than
fabricating an empty or partial block. An older client that sends no check inputs remains valid;
an older client that resolves a checked row without the answer settles it and leaves the check
pending and answerable (ACC-06, ACC-20) — it cannot record, replace or read a result.

*Superseded 2026-09-24.* The wording this replaces read: "an older client cannot bypass a check
because ACC-06 refuses checked-row resolution without the answer." ACC-06 no longer refuses
(George, 2026-09-24, `t-1aa9f553`); the check is deferred rather than bypassed, because its one
answer can still be recorded after resolution.

### Miss-rate report

**ACC-18 — Report resolved check results by subject from the CLI.**
Strength: MUST · Layer: process · Source: `t-b6bfaf40` requesting body; the operator aggregate
read in §2 Users / actors.
`attention list --check-report [--all] [--all-boards] [--limit N] [--json]` (spelled `kb att
list --check-report …` through the skill alias; `att` aliases `attention` at
`rust/lib.rs:304`) reads only resolved rows that carry a native check with a recorded answer —
the `answered`/`correct` result fields from ACC-07 (`rust/model.rs:1043`-`:1049`) — and groups
them by `about`. Rows without a check and rows without a recorded answer contribute nothing.
Each group carries `about`, the answered count, the correct (pass) count, the missed count and
the miss rate (missed ÷ answered, whole-percent integer truncated toward zero); groups sort worst first — miss rate
descending, then missed descending, then `about` ascending. The group key is `about` alone:
no raiser, actor, answer-key, explanation or choice-label column exists in either shape. The
table prints the five columns in order `about`, `answered`, `correct`, `missed`, `miss-rate`;
`--json` returns an array of `{about, answered, correct, missed, missRate}` objects and nothing
else. `--status` beside `--check-report` is refused naming the conflict, because the report
fixes status to resolved; the remaining row filters (`--kind`, `--task`, `--tag`, `--lane`
from the `attention list` flag table at `rust/lib.rs:1324`-`:1332`) still narrow the resolved
set, and the row-shape flags (`--fields`, `--no-body`) are refused beside `--check-report`
naming the report's fixed columns. The listing is capped like every listing (ADR-037): the
default bound is 100 groups through the same `bounded_page` helper `attention list` uses
(`rust/lib.rs:7286`), fetching one group past the bound; more groups than the default with no
`--limit` refuses with the listing sentence — `found more than 100 check subjects and no
--limit was given — a page cut at the default would read as the whole; pass --limit N, above
100 to see more or exactly 100 to take the first 100 knowingly` (template at
`rust/lib.rs:2241`-`:2245`) — and an explicit `--limit` returns the first N groups in
worst-first order silently with exit zero (`rust/lib.rs:2234`-`:2235`). A `--limit` below 0
or above the 1000000 ceiling is refused by the existing limit law (`rust/lib.rs:2198`,
`rust/lib.rs:2203`-`:2207`). `--all` keeps its `attention list` meaning of including archived
rows (`rust/lib.rs:7294`); beside `--all-boards` it additionally includes retired boards, as
the search listing does. `--all-boards` fans out through the registry exactly as the search
listing does: a read-only registry open, active boards unless `--all` (`rust/lib.rs:3780`
-`:3787`), board selectors refused beside it (`rust/lib.rs:3711`-`:3714`), and unreadable or
missing boards reported rather than silently skipped (`rust/lib.rs:3794`-`:3800`). A board
with no resolved checked rows prints the header with zero groups (an empty array under
`--json`) and exits zero.

**ACC-19 — Summarize resolved check results in one block on /decided.**
Strength: MUST · Layer: chrome · Source: `t-b6bfaf40` requesting body.
The `/decided` page carries exactly one compact block above the decision rows, aggregated by
the same by-`about` grouping as ACC-18 from the page's own already-read items — the merged
`DecidedListing` rows from `projection::decided()` (`rust/projection.rs:605`-`:623`), which
are `Store::recent_resolved_attention` rows (`rust/store.rs:6626`) cut to `DECIDED_ROWS`
(`rust/serve.rs:82`) on the `["decided"]` route (`rust/serve.rs:514`-`:515`). No new page, no
new route and no new Store read exist. The block's sentence is byte-exact in shape: `Checks:
N answered, M missed — worst: <about> (x/y)` where N is the answered resolved checks among
the page's items, M the missed subset, and `<about> (x/y)` the worst group under ACC-18's
order with its missed (x) and answered (y) counts. The worst `about` links to that subject's
rows on the page through their `#d-<id>` anchors (row headings carry `id="d-<id>"` at
`web/src/pages/decided.tsx:164`); the block root carries `data-testid="decided-check-summary"`
and the sentence node carries `data-testid="decided-check-summary-text"`. When the page holds
no resolved checked rows the block is omitted and the existing empty page
(`data-testid="decided-empty"` at `web/src/pages/decided.tsx:139`) reads unchanged. The block
carries post-answer aggregate data only — `about` strings and counts; it never carries answer
keys, explanations, choice labels or raiser identity, and redaction otherwise follows ACC-13.

### Deferred answers

**ACC-20 — Accept the one answer while the row is open or resolved.**
Strength: MUST · Layer: process · Source: George, 2026-09-24 on `t-1aa9f553`; ACC-08.
A check whose answer is not yet recorded accepts its one answer through the shared Store answer
operation whether its row is open or resolved. Recording the answer changes neither the row's
status, decision nor resolution text, and is not a card edit under ACC-05: the definition stays
as authored. Every other refusal of ACC-07, ACC-08 and ACC-14 is unchanged — a row with no check,
an already-recorded answer (identical or different key) and an undeclared key are refused
without mutation. Reopen still clears the recorded result (ACC-05), after which the check
accepts one new answer.

**ACC-21 — Answer a check from the CLI and teach with the receipt.**
Strength: MUST · Layer: process · Source: George, 2026-09-24 on `t-1aa9f553`; ACC-11.
`attention check ID --as ACTOR --key KEY [--json]` (spelled `kb att check …` through the skill
alias) records the one answer through the same Store answer operation as the ACC-11 web route; it
cannot implement a second answer law. Only `geoyws` or the row's `raisedBy` may run it, as with
resolve, and the refusal names the raiser. The `--json` receipt is the row's post-answer
projection: `check` carries `question`, `choices`, `about`, `answer` (the correct key),
`explanation`, `answered`, `correct` and `answeredAt`. The text receipt prints three lines, for
a pass as much as a miss: `ACC: pass` or `ACC: miss on <key>` (agreeing with the stored result as
ACC-07's echo does), then `answer: <correct key> — <its label>`, then `why: <explanation>`.

## 4. Acceptance examples

### A1 — valid full block (`ACC-01`, `ACC-02`, `ACC-03`, `ACC-05`)

*Given* a raiser creates a carded attention row with a question of 160 characters ending `?`,
three in-bound choices, an answer naming one choice, a 400-character explanation naming the
component and `about=src/store.rs`,
*when* `attention raise` writes it,
*then* that successful same-write receipt may echo the complete definition, no recommendation or
outcome exists on the check or its choices, and a later `att show --json` omits answer and explanation.

### A2 — partial, bounds and answer mismatch (`ACC-01`, `ACC-02`)

*Given* no row exists, *when* raise is attempted separately with a partial block, a 161-character
question, a question without `?`, one or five choices, an over-bound existing choice key/label,
a 401-character explanation, or an answer key not among the choices, *then* each request is
refused naming its field and no row or event is written.

### A3 — subject-shape mismatch (`ACC-02`)

*Given* a syntactically complete check, *when* `about` has none of the three approved subject
shapes, *then* the write is refused naming all three shapes and no prior row data changes.

### A4 — diagnosis shape (`ACC-04`)

*Given* an otherwise valid check, *when* each agreed typed-row-ID prefix, an absolute date and
each agreed phrase is placed in turn in the question, a choice and the explanation, *then* the
write is refused before persistence, prints the offending token and prints verbatim: `a check
teaches how the system works; it never reads this row's finding back.`

### A5 — non-raiser update and resolve definition (`ACC-05`)

*Given* the raiser stored a valid check, *when* another lane, a clerk or `geoyws` supplies any
check-definition input to update, or any actor tries to add/change a definition through resolve,
*then* the request is refused naming the raiser and the definition and event history are unchanged.
### A6 — missing or already-recorded answer (`ACC-06`)
*Given* an open checked row with no recorded answer, *when* resolve omits `--check-answered`,
*then* the row resolves, no result or `ACC:` echo is written and the check stays pending and
redacted on show and list; *given* the answer is already recorded, *when* web or CLI resolves
without a second answer flag, *then* resolution uses the recorded result, asks nothing twice and
succeeds.

*Superseded 2026-09-24.* The example this replaces read: "*when* resolve omits
`--check-answered`, *then* it is refused with the question, the row remains open and no
decision, result or event is written". See ACC-06.

### A7 — right answer (`ACC-07`)

*Given* an open checked row whose answer is `b`, *when* resolution supplies `b`, *then*
`answered=b`, `correct=true`, `answeredAt` and `ACC: pass` agree and the row resolves.

### A8 — wrong answer (`ACC-08`)

*Given* an open checked row whose answer is `b`, *when* resolution supplies declared key `a`,
*then* `answered=a`, `correct=false`, `answeredAt` and `ACC: miss on a` agree, the row resolves,
and no attempt counter or retry path exists.

### A9 — check-first browser journey (`ACC-09`, `ACC-12`)

*Given* a checked card in real Chrome, *when* it first renders, *then* its subject, question and
answer controls are first, decision controls cannot submit, and check controls work by keyboard
and pointer; *when* the server records an answer, *then* the decision controls unlock, a miss is
announced with its explanation, and the existing decision hotkeys and Undo work unchanged.

### A10 — browser source redaction (`ACC-10`)

*Given* the secret answer and explanation contain unique sentinel strings, *when* real Chrome
loads the unanswered card and every browser-available HTML/JSON/script/bundle response is read,
*then* neither sentinel exists; *when* the answer is posted, *then* only the selected post-answer
projection may carry them and public refusal wording never names the answer key.

### A11 — authorized and unauthorized HTTP (`ACC-11`, `ACC-14`)

*Given* one authorized and one unauthorized actor address the same checked row, *when* each posts
the same key and reads the row, *then* the authorized actor reaches the shared Store operation and
the unauthorized actor receives the existing non-enumerating denial, learns no check metadata
and writes no result or event.

### A12 — every pre-answer reader is redacted (`ACC-13`)

*Given* an unanswered checked row, *when* authorized list JSON, MCP, digest, HTTP and any actor's
own show — including the raiser's — are read, *then* each carries only question, choices and
`about`; explanation and answer fields do not exist. The successful raise write receipt is the
only pre-answer response allowed to echo the complete definition. *When* the answer is recorded,
*then* permitted reads may carry explanation, answer and stored result.

### A13 — native skill cutover (`ACC-15`)

*Given* one native checked row and one row containing only legacy/free prose, *when* `/kb-att`
builds its digest, *then* it asks only the native check first and never synthesizes a check from
either row's context or body.

### A14 — migration and no-check compatibility (`ACC-16`, `ACC-17`)

*Given* a version-boundary board with one valid leading `ACC:` block, one row without a block and
one ordinary no-check row, *when* migration runs twice and an older no-check client lists and
resolves the ordinary row, *then* the first row has one native check, the other rows are unchanged,
the second migration adds nothing, and no-check projection and resolution remain as before ACC.

### A15 — retry, duplicate and concurrency (`ACC-08`, `ACC-11`)

*Given* one answer has been accepted, *when* the identical or a different key is submitted again,
*then* the second write is refused and stored answer/result are unchanged; *given* two concurrent
submissions, *when* they serialize, *then* exactly one succeeds and the loser returns the existing
web conflict or CLI refusal after observing the winner. Retry is N/A: there is no retry loop.

### A16 — authoring across answer, resolution and reopen (`ACC-05`)

*Given* an answered open row, *when* its raiser tries to rewrite the check, *then* the edit is
refused and definition, answer/result and unlocked decision choices are unchanged; *given* a
resolved checked row, *when* its raiser tries the same edit, *then* the immutable-card refusal is
returned; *when* the row is reopened, *then* current decision/check result clear, decision choices
lock behind a new answer and raiser update is restored.

### A17 — miss-rate report from the CLI (`ACC-18`)

*Given* a board holding resolved checks across three subjects — `src/store.rs` with 5 answered
and 4 missed, `@@edge` with 4 answered and 1 missed, `FAST_FLAG` with 3 answered and none
missed — *when* `kb att list --check-report` runs, *then* three groups print worst first with
miss rates 80, 25 and 0, and `--json` returns the same three objects with exactly the keys
`about`, `answered`, `correct`, `missed` and `missRate` and no raiser, actor, answer-key,
explanation or choice-label field. *When* `--limit 2` is passed, *then* only the first two
groups print with exit zero; *when* `--limit -1` or `--limit 1000001` is passed, *then* the
existing limit refusal is returned; *when* more than 100 subjects exist and no `--limit` is
given, *then* the listing refuses naming `--limit`. *Given* a board with no resolved checked
rows, *when* the report runs, *then* the table prints its header with zero groups (an empty
array under `--json`) and exits zero.
*Given* a subject with 6 answered and 1 missed, *when* the report runs, *then* its miss rate
prints 16, pinning truncation toward zero. *Given* the A17 board, *when* `--status open` is
passed beside `--check-report`, *then* the run is refused naming the conflict; *when* `--kind`
narrows the resolved set, *then* only matching rows aggregate; *when* `--fields` or `--no-body`
is passed beside `--check-report`, *then* the run is refused naming the report's fixed columns.

### A18 — miss-rate block on /decided (`ACC-19`)

*Given* the A17 board, whose twelve answered resolved checks (five missed) all fit the page's
newest-20 cut, *when* real Chrome loads `/decided`, *then* exactly one block with
`data-testid="decided-check-summary"` reads `Checks: 12 answered, 5 missed — worst:
src/store.rs (4/5)`, the worst subject links to its `#d-<id>` rows, and no answer key,
explanation, choice label or raiser name occurs in the block. *Given* a board with no
resolved checked rows, *when* `/decided` loads, *then* the block is omitted while the
existing empty page reads unchanged.

### A19 — deferred answer on a resolved row (`ACC-06`, `ACC-20`, `ACC-21`)

*Given* a checked row whose answer is `fields` (label `Stored fields on the attention row`)
resolved with no `--check-answered`, *when* `attention check ID --as geoyws --key notes --json`
runs, *then* the receipt reads `status=resolved`, `answered=notes`, `correct=false`,
`answer=fields` and carries the explanation, and the ledger records `attention_check_answered`
without the explanation; *when* either key is submitted again, *then* it is refused and the board
and audit chain are unchanged. *When* the row is reopened and `attention check ID --as geoyws
--key fields` runs without `--json`, *then* stdout is exactly `ACC: pass`, `answer: fields —
Stored fields on the attention row` and `why: <explanation>` on three lines and the row is open.
*Given* an open checked row, *when* its raiser answers it, *then* it stays open and a later bare
resolve reuses the result with no echo. *When* the row has no check, the key is undeclared, or
the actor is neither `geoyws` nor the raiser, *then* the command is refused naming why and
nothing changes.

### A20 — search scores depend on permitted documents only (`ACC-14`)

*Given* a principal with board read and write on untagged rows plants an anchor task carrying
a unique token once in a long body, *when* they search the anchor token beside a prefix no
visible document matches, *then* the anchor's served `lexicalScore` and `score` read some
value; *when* a tag-denied document strongly matching that prefix is added, *then* the same
query serves the anchor with byte-identical `lexicalScore` and `score`, and the denied row
appears in neither receipt. On an unenforced board the same query is unaffected by design:
every row is readable there, so the divisor is still taken over the whole match set.

### A21 — task-attach writes inherit the task's tags (`ACC-14`)

*Given* a managed caller holding board read and write but no `secret` tag
scope, *when* it attaches to a `secret` task through `note`, `attention raise
--task`, `sitrep post --task`, `checkpoint`, `handoff create` with a task,
`task add --parent` or `--depends-on`, `task update --parent` or `--depends-on`,
`subscription add --subject` or `--relation`, or `deploy start --task`, *then*
each write is refused with the existing non-enumerating denial — byte-identical
in exit code and stderr to the same write naming a never-created id — and nothing
is recorded: an owner re-read shows each task-scoped ledger holding only its birth
event, the phantom children absent, and no subscription created. *When* no
enforcement applies, *then* an unknown id keeps its plain `not found` message and
authorized attaches work as before.

### A22 — a removed task's trail stays tag-gated on every tail (`ACC-14`)

*Given* an untagged task whose body edit leaves a unique token in a
*`task_updated.previousBody` while its snapshot still reads `tags: []`,
*when* the task is tagged `secret` and then removed, *then* a managed
*caller holding only board read sees the token on no tail — not in
*board-wide `events`, not in a `watch --follow` stream, not in `search` —
*while `events --task` on the gone row refuses exactly like a
*never-created id, and the same caller granted `secret` read still reads
*the token in `events`, in `watch --follow` and in `search` alike.
*Removal never loosens a tag: a removed task's events authorize against
*the task's last-known tags from its `task_removed` snapshots plus each
*event's own snapshot, so the tails and the index agree instead of the
*index dropping what the tails serve.

### A23 — a removed task's linked rows stay tag-gated on every surface (`ACC-14`)

*Given* a managed caller holding board read and write but no `secret` tag
scope, *when* the owner posts a sitrep `--task`, creates a handoff, raises an
untagged attention row and starts a deployment `--task` on a `secret` task
and then removes the task, *then* the caller sees none of them in any
listing, in search, in a lane-filtered sitrep listing, on the deployment
listing or in a `watch --follow` stream, and `handoff retire`, `handoff
accept`, `attention show`, `deploy show` and `deploy finish` answer exactly
as for a never-created id — the same exit code and byte-identical stderr
carrying the generic denial, with nothing recorded — while genuinely
taskless rows (a lanewide sitrep, a session handoff, a taskless deployment,
an untagged attention row) stay readable to the same caller and the owner
still sees every row. *When* no enforcement applies, *then* an unknown id
keeps its plain `not found` message. The board schema moves `35` → `36`:
the four links become plain `TEXT` with no foreign key, so a removal keeps
the orphaned id and every read path gates it on the task's last-known
removal tags. Rows already nulled by an older removal recover their link
from their creation event — but only when that event's task has a
`task_removed` record and no live row — so session handoffs, lanewide
sitreps and taskless deployments stay NULL. Accepting an orphaned handoff is
an acknowledgement that mints no lease, and the archive sweep files orphaned
handoffs and events away once they are old. A deployment naming a task with
neither a live row nor a removal record hides just that row instead of
failing the listing, and `doctor` reports it under `orphanedTaskLinks`.

### A24 — a removed task's id is never reused (`ACC-14`)

*Given* a managed caller holding board read and write but no `secret` tag
scope, *when* the owner posts a sitrep on a `secret` task and then removes
the task, *then* `task add --id` naming the removed id is refused exactly
like `task add --id` naming a live `secret` id — the same exit code and
byte-identical stderr carrying the generic denial, with nothing recorded —
and the caller sees the surviving sitrep on no listing, while the owner
still reads it. *When* no enforcement applies, *then* re-adding a removed id
answers `task X was removed and its id cannot be reused`, and re-adding a
live id answers `task X already exists`, replacing the raw `UNIQUE
constraint failed`. Removed ids stay unreusable on every board, managed or
not, through `task add` and through import, because the rows that outlive a
removal keep the orphaned id and would otherwise re-parent under the new
row's tags.

### A25 — a dependency replacement keeps the edges the caller cannot read (`ACC-14`)

*Given* the owner gates a `visible` task `t-visible` on a `secret` task
`t-secret` and on an `ops` task `t-ops`, *when* a managed caller holding board
read and write but no `secret` tag scope and only `ops` read runs `task update
t-visible --depends-on t-other` for a readable `t-other`, *then* the write
succeeds — refusing would confirm a hidden edge exists — but both kept edges
survive: the caller's own `task show` still lists `t-ops` beside `t-other`
while withholding `t-secret`, the update receipt names no hidden id, and
`blockingGates` still carries both `t-secret` (with its title blanked) and
`t-ops` as `todo`, so `claim t-visible` is still refused while either is open.
Re-listing `t-ops` beside `t-other` is accepted — a readable edge already on
the row needs no new authority, as with the unchanged parent — while
naming `t-secret` in the new list is refused byte-identically to naming an
id that was never an edge (the same exit code and stderr, nothing written):
the skip applies only to edges the caller can read, so guessing a hidden
prerequisite confirms nothing, and the kept loop still preserves the hidden
edge on every accepted replacement. `--clear-dependencies` keeps both the
hidden and the read-only edges while dropping the writable `t-other` edge,
because removing a gate is a write
against the prerequisite's scope as much as the dependent's. The owner still
reads every surviving edge and can still drop any of them. *When* no
enforcement applies, *then* a replacement rewrites the whole list as before,
because there is no hidden or read-only edge to keep.

### A26 — pages are bound by readable rows, not raw rows (`ACC-14`)

*Given* a managed caller holding board read and the `visible` tag but no
`secret` scope, *when* the two newest events
concern a `secret` attention row raised and settled on the readable task
`t-visible` and the newest deployment attempts name the denied task
`t-secret`, *then* `events --task t-visible --limit 1` and board-wide
`events --limit 1` each return the newest visible event with the `showing 1
of more than 1` truncation notice, and `deploy list --status started --limit 1`
and `--status failed --limit 1` each return the newest visible attempt —
none of them naming the hidden rows — while the owner still reads the
denied rows first on every listing, and granting
`secret` restores exactly those rows to the caller. *When* three hundred
denied events sit ahead of the visible history, *then* board-wide `events
--limit 5` still returns the five newest visible events newest-first, with
no duplicates and the `showing 5 of more than 5` notice. *When* a restricted
follower polls the tail over that denied stretch, *then* each scan examines
a bounded page, the poll's static walk starts at the scan frontier so even a
`--limit 1` follower advances by the scan floor, the cursor advances past
the denied rows already read, and the visible event waiting past the stretch
is still delivered — a delay, never a skip, and never a re-read of the whole
stretch. *When* a one-shot (non-follow) watch lands behind a denied stretch
longer than the raw cap, *then* it emits one `advanced` heartbeat carrying
the scan frontier, so a polling consumer re-running from the persisted
cursor walks the next page instead of re-reading the same rows. Authorization is at scan
time throughout: rows denied under the authority and tags current at the
scan stay behind the adopted cursor, and a later grant or retag does not
replay history already passed. *When* no enforcement applies, *then* the
listings keep their SQL bound and read exactly what they asked for,
byte-identical.

### A27 — denied and unknown ids answer identically on every by-id surface (`ACC-14`)

*Given* a managed caller holding board read and write but no `secret` tag
scope, *when* it addresses a `secret` attention id and a never-created id on
CLI `attention show`,
`attention resolve`, `attention reopen`, `attention check` and `task show`,
*then* each pair answers byte-identically — the same exit code and stderr —
with the existing
non-enumerating denial, and nothing is recorded. *When* the same caller
answers a readable row's check with an undeclared key, *then* the refusal
names the key rather than collapsing into the denial; *when* no enforcement
applies, *then* an unknown id keeps its plain `not found` message.

### A28 — denied and unknown task ids answer identically on task routes (`ACC-14`)

*Given* a managed caller holding board read and write but no `secret` tag
scope, *when* it addresses a `secret` task id and a never-created one on
`task move`, `task update`, `task remove`, `claim`, `events --task` and
`deploy show`, *then* each pair fails with the same exit code and
byte-identical stderr carrying the existing non-enumerating denial, and
nothing is recorded. *When* it filters `attention list --task` by either id,
*then* both succeed alike. `notes`, `checkpoints` and a named `claim` have
no standalone CLI read — every command reaches them only past `require_task`
— so a store-level case pins the same denial for both ids there. *When* no
enforcement applies, *then* an unknown task keeps its plain `not found`
message.

## 5. Contracts and data

- **Interface version or schema:** CLI adds the five definition inputs,
  `attention resolve --check-answered KEY`, `attention check ID --as ACTOR --key KEY [--json]` and
  `attention list --check-report [--all]
  [--all-boards] [--limit N] [--json]`; MCP exposes their typed equivalents; HTTP adds
  `POST /attention/{project}/{id}/check` to the repository's pinned OpenAPI document. `/decided`
  gains one summary block on its existing route with no new endpoint. OpenAPI is
  applicable because this is an HTTP operation; it is not a substitute for CLI/MCP schemas.
- **Data invariants:** a definition is absent or complete; it contains question, two through four
  choices, one declared answer, explanation and `about`; result data contains
  `answered`/`correct`/`answeredAt` consistently or is absent; no check choice has recommendation
  or outcome semantics. Attempt data is prohibited. An answered open row's check definition and
  result are immutable until resolution; reopen atomically clears the current decision/result and
  restores the unanswered open-row state.
- **Migration:** schema bump at the ADR-045 boundary plus the one-shot, receipt-bearing conversion
  in ACC-16; forward migration only. Rows without a valid leading legacy block are unchanged.
- **Compatibility:** absent checks remain absent and preserve old read/resolve behaviour; old
  resolvers that omit the answer settle a checked row and leave its check pending (ACC-06,
  ACC-17); redaction is an omission, not a renamed or nullable answer field.
- **Read projection:** before answer recording, every show, list, MCP, digest and HTTP read omits
  answer and explanation for every actor, including `raisedBy`. A successful raise receipt is a
  same-write acknowledgement, not a later read, and is the sole allowed pre-answer full-definition
  echo.
- **Ownership:** the containing board Store owns definition, result and authorization; `raisedBy`
  owns authoring permission subject to ACC-05's lock. Skills and web are projections, not
  alternate ledgers.
- **Events/audit:** the approved sources require persisted result fields and a matching human note
  echo but do not approve a new event type or payload name. Existing write auditing applies; this
  specification deliberately invents none.

## 6. Quality and security

- **Reliability:** validation and authorization occur before a write; a refusal leaves row, result,
  decision and event history unchanged. Restart preserves accepted native fields. Concurrent
  answers serialize; exactly one first answer persists and every loser is refused unchanged.
- **Accessibility:** ACC-12 requires labelled controls, keyboard/pointer equivalence, announced
  state/explanation and non-colour-only feedback while preserving the existing hotkeys.
- **Privacy:** check contents inherit row tenancy and tag visibility. The answer and explanation
  are product-sensitive assessment data: before answer recording every read surface omits both for
  every actor, including the raiser; only the successful same-write raise receipt may echo them.
  No new retention period is invented.
- **Security — ASVS applicability:** OWASP ASVS **5.0.0** is selected as verification guidance,
  not a certification claim. **V2.2.1** and **V2.2.2** apply to bounded/structural validation at a
  trusted service layer (ACC-01/02/04); **V8.3.1** applies because authorization must be enforced
  by the trusted Store rather than browser controls (ACC-05/14); **V14.2.6** applies because HTML,
  JSON, show/list, MCP, digest and HTTP reads must disclose only the minimum data and never the
  pre-answer key or explanation to any actor (ACC-10/13). Authentication, cryptography and session-
  management chapters add no ACC-specific contract because this slice inherits the existing
  trusted-edge identity and creates no secret, credential or session mechanism.
- **Operability:** migration records one receipt per board; native result fields support reporting
  pass/miss/none by `about`; invalid legacy content is reported rather than half-written.
- **Performance:** no performance or latency target was approved, so none is specified.

## 7. Open questions

None.

**Non-goal, not a question.** A leaderboard and per-raiser scoring are excluded by §2 and
carried by ACC-18 and ACC-19: the report groups by `about` alone and has no actor or raiser
dimension to score. Nothing here asks who is behind, so nothing blocks on it.

**Resolved question history.** OQ-1 asked whether the raiser may rewrite an answered check while
the row remains open. George chose **lock** (the recommended choice) on decision card
`a-db9de88b` for `t-6a3dd1a6` on 2026-09-21: refuse the edit and preserve the definition,
recorded answer/result and unlocked choices until normal resolution. ACC-05 carries the contract.
**Resolved projection history.** ACC-13 and A12 originally allowed the raiser's later `att show`
to return the full unanswered definition. That requirement was superseded on 2026-09-21 because
`--as` is self-declared and `AuthzContext` carries no authoritative principal-to-`raisedBy`
binding. George chose safe redaction on every later show/read instead of adding a principal
binding or token. The successful raise response remains a same-write receipt and may echo what
its author just supplied.
**Resolved scope history.** ACC-06 originally refused to resolve a checked row without its
answer, and the Store refused to answer a check on a resolved row. George chose "Ledger + skills"
on 2026-09-24 (`t-1aa9f553`): resolve no longer waits for the check, the one answer is accepted
open or resolved (ACC-20), and the CLI gained `attention check` (ACC-21) so `/kb-acc` can ask
checks apart from `/kb-att` and correct every answer. ACC-06, ACC-15, ACC-17 and A6 carry dated
`Superseded` paragraphs quoting the replaced wording. The web card's check-before-decision order
(ACC-09) is unchanged.

## 8. Verification

The trace of record has one row for every mandatory ACC requirement. A row names implementation
evidence only after it lands; incomplete requirements remain explicitly `PARTIAL` or `PLANNED`.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `ACC-01` | MUST | process | `PLANNED` | full block plus partial/bounds/answer-key refusals |
| `ACC-02` | MUST | unit | `check_about_accepts_only_the_three_approved_subject_shapes` | path/file, host/tier sigil and flag/default shapes plus byte-exact mismatch sentence |
| `ACC-03` | MUST | unit | `PLANNED` | no recommendation, outcome or decision mutation |
| `ACC-04` | MUST | unit | `diagnosis_markers_are_refused_in_every_checked_text_location_with_the_exact_rule`; `diagnosis_markers_respect_case_and_word_boundaries`; `native_check_refuses_a_diagnosis_shaped_raise_then_accepts_the_rewrite` | every approved marker in question, choice label and explanation with the byte-exact rule sentence; case/word-boundary edges plus the compiled raise-refuse-then-rewrite e2e; `about` is intentionally outside this marker rule |
| `ACC-05` | MUST | process | `answered_check_locks_definition_and_a_later_resolve_reuses_it`; `native_check_store_round_trip_redaction_authorization_and_atomic_update`; `attention_check_update_is_raiser_only_across_the_three_actors` | raiser-only authoring and resolved immutability landed 2026-09-21; answered-open lock and reopen-clear/restore landed 2026-09-22; the three-actor update pin (raiser, another lane, `geoyws`) and the resolve-takes-only-`--check-answered` refusal land with `t-1227a592` |
| `ACC-06` | MUST | process | `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `answered_check_locks_definition_and_a_later_resolve_reuses_it`; `attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits` | as amended 2026-09-24: a bare resolve settles a checked row with no result or echo and the check stays redacted on the receipt, show and resolved list; no-check/undeclared-key paths; a recorded answer lets a later resolve settle with no second flag |
| `ACC-07` | MUST | process | `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `v32_result_columns_are_absent_complete_and_defined` | persisted answer/correct/time, note agreement, v32 all-or-none columns |
| `ACC-08` | MUST | process | `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `answered_check_locks_definition_and_a_later_resolve_reuses_it` | right/wrong paths and immutable duplicate; serialized concurrent answers land with the ACC-11 endpoint |
| `ACC-09` | MUST | chrome | `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | check-first card with inert decision, one answer, unlock and miss explanation |
| `ACC-10` | MUST | chrome | `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | page and projection sentinel sweep; reveal only post-answer |
| `ACC-11` | MUST | http | `the_check_card_answers_before_the_decision_and_never_leaks_the_key`; `answered_check_locks_definition_and_a_later_resolve_reuses_it` | shared POST through the one Store operation; the serialized-loser half is held by the store-level one-answer refusal |
| `ACC-12` | MUST | chrome | `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | keyboard and pointer equivalence, digit ownership, focus move, worded pass/miss, Undo preserved |
| `ACC-13` | MUST | process | PARTIAL — `native_attention_check_round_trips_rewrites_redacts_and_refuses_atomically`; `native_check_store_round_trip_redaction_authorization_and_atomic_update`; `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | every pre-answer show/list and mutation receipt omits answer/explanation even for the raiser, and the HTTP projection sweep pins the same omission in the browser bytes; after the answer is recorded show/list carry answer, explanation and result, and a reopen redacts again; the digest projection half stays unproven — no digest test in the tree (t-0382c937, 2026-09-29) |
| `ACC-14` | MUST | process | `checked_row_stays_non_enumerating_to_an_unauthorized_actor`; `search_scores_are_a_function_of_permitted_documents_only`; `note_attention_raise_and_sitrep_refuse_a_tag_denied_task_like_an_unknown_id`; `a_removed_tasks_trail_stays_tag_gated_on_every_tail`; `removed_task_links_stay_tag_gated_on_every_listing_search_and_lane`; `an_orphaned_handoff_stays_deniable_yet_acceptable_and_archivable`; `removed_task_ids_are_never_reused_and_probe_like_live_denied_ids`; `reusing_a_task_id_is_refused_with_a_plain_message_where_no_guard_can_deny`; `compiled_binary_accepts_a_handoff_on_a_removed_task_and_archives_it`; `compiled_binary_hides_an_orphan_deployment_and_doctor_reports_it`; `store::tests::managed_deployment_listing_hides_an_orphan_link_and_doctor_reports_it`; `schema_36_keeps_task_links_without_foreign_keys`; `schema_36_backfills_pre_v36_nulled_links_from_creation_events`; `task_attach_writes_refuse_a_tag_denied_task_like_an_unknown_id`; `managed_pages_fill_past_denied_rows_with_a_true_truncation_probe`; `watch::tests::a_limit_1_follow_poll_advances_by_the_scan_floor_over_denied_rows`; `watch::tests::a_one_shot_watch_behind_denied_rows_reports_progress_not_silence`; `watch::tests::an_unenforced_one_shot_watch_stays_silent_behind_rejected_rows`; `denied_and_unknown_ids_answer_identically_on_every_by_id_attention_surface`; `denied_and_unknown_task_ids_answer_identically_on_task_routes`; `store::tests::managed_notes_checkpoints_and_named_claim_deny_denied_and_unknown_tasks_identically` | same-key check post, show and answering resolve on another board's checked row all receive the generic denial with no question, choice, answer, explanation or `about` anywhere, and the check stays unanswered with no result afterwards; a tag-denied document moves no permitted hit's served `lexicalScore` or `score`; A11's HTTP half stays unexercised (t-2e2ea981, t-e9c0127a); note, attention raise `--task` and sitrep post `--task` answer a denied id and a never-created id byte-identically under a managed principal, record nothing, and keep plain not-found messages unmanaged (t-d2fd604a, A21); a removed task's trail stays tag-gated on board-wide `events`, `watch --follow` and `search`, and `events --task` on the gone row refuses exactly like a never-created id (t-bd66208d, A22); a removed secret task keeps its sitrep, handoff, attention row and deployment attempt gated on its removal tags across every listing, search, lane-filtered sitreps, watch and every by-id surface while taskless rows stay readable (t-2cffbe08, A23); an orphaned handoff stays deniable yet acceptable without a lease, archivable, and doctor-healthy; a removed id and a live denied id probe identically on `task add --id`, with plain refusals where no guard can deny (t-2cffbe08, A24) |
| `ACC-15` | MUST | process | SUPERSEDED 2026-09-24 | Ledger + skills scope change (`t-1aa9f553`): the native-cutover wording is replaced; live behaviour is ACC-20/ACC-21 |
| `ACC-16` | MUST | process | `migrate-acc-body-blocks.sh` + `migrate-acc-body-blocks.test.sh`, wired at `scripts/release-gate.sh:93`; `schema_30_migrates_once_to_native_check_columns_without_inventing_a_check` | one-shot conversion of valid legacy `ACC:` blocks with operator receipt; rows without a block byte-for-byte unchanged; rerun migrates nothing; invalid prose reported for hand migration (t-0382c937, 2026-09-29) |
| `ACC-17` | MUST | process | SUPERSEDED 2026-09-24 | resolve-no-longer-waits (`t-1aa9f553`); live behaviour is ACC-06/ACC-20 |
| `ACC-18` | MUST | process | `att_list_check_report_groups_worst_first_with_adr037_caps`; `att_list_check_report_fans_out_across_boards`; `aggregate_check_report_groups_worst_first_with_truncation_and_skips` | A17 table values, truncation case, JSON keys, limit/cap refusals, status/filter/shape-conflict refusals and empty board; registry fan-out with the board-selector refusal; store-level worst-first, truncation and skip unit |
| `ACC-19` | MUST | chrome | `decided_page_carries_one_check_summary_block` | A18 sentence shape, row links, test ids and omission on empty, plus the `/api/v1/decided` `checkSummary` projection beside the page's rows |
| `ACC-20` | MUST | process | `attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits`; `answered_check_locks_definition_and_a_later_resolve_reuses_it` | one answer on a resolved row and on an open row, status/resolution unchanged; identical and different second answers refused with board and audit chain unchanged; reopen clears and the check answers again |
| `ACC-21` | MUST | process | `attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits` | compiled `attention check` JSON receipt with answer, explanation and result; byte-exact three-line text receipt on a pass; raiser allowed, non-raiser non-`geoyws` refused naming the raiser, no-check and undeclared-key refusals |

## 9. Change log

- 2026-09-21 — initial ACC specification drafted from the eight approved board sources; the later
  approved child rows' one-answer/no-retry contract supersedes the epic draft. Independent review
  removed an unapproved semantic classifier, applied later reader redaction and inherited write/
  lifecycle laws. George chose **lock** on `a-db9de88b`; ACC-05 now refuses answered-open edits,
  closing the sole open question and making the specification `SPEC-READY`.
- 2026-09-21 — George superseded raiser-full pre-answer show with always-redact reads. The
  discarded design relied on self-declared `--as` while `AuthzContext` had no principal binding;
  he chose safe omission rather than adding a binding/token. ACC-13, A1, A12 and the data/security/
  trace contracts now preserve only the same-write raise-receipt exception.

- 2026-09-22 — the answer slice landed: `attention resolve --check-answered` records the one
  answer as v32 columns (`check.answered`, `check.correct`, `check.answeredAt`), a checked row
  refuses a bare resolve naming its question, an already-recorded answer settles later
  resolves, reopen clears the result with the decision, and the answered-open definition lock
  from ACC-05 took effect. Requirement wording is unchanged; only §8 trace rows gained
  evidence. The v31 rebuild guard in the migration ladder was pinned to the v31 step — it had
  hardcoded "last migration" and would have silently skipped every later migration.

- 2026-09-22 — the web card slice landed: the deck renders the check before an inert
  decision (a disabled fieldset, never a withheld click), `POST /attention/{project}/{id}/check`
  records the one answer through the same Store operation the CLI's `--check-answered`
  resolve uses, and a real-Chrome journey sweeps the pre-answer page and projection for the
  answer, explanation and result fields. Requirement wording is unchanged; only §8 trace
  rows gained evidence. The route joined ADR-016's write allowlist as its one later
  addition, documented in the pinned OpenAPI.

- 2026-09-23 — ACC-18 (CLI miss-rate report) and ACC-19 (`/decided` summary block) appended
  at the end of the creation sequence for `t-b6bfaf40`, the ADR-047 §6 edited-slice path for
  specified-before-implementation: read-only aggregates over the resolved check result fields,
  no new tables and no wording change to ACC-01..17. Leaderboard and per-raiser scoring are
  recorded as non-goals in §2 and §7.
Review fixes the same day: miss rate pinned to truncation toward zero with an A17 1/6 case;
A17 extended to the status-conflict, row-filter and shape-flag refusals; §2 teaching-purpose
and no-target bullets restored verbatim beside the leaderboard bullet.

- 2026-09-24 — owner-authorised scope change for `t-1aa9f553` (George, "Ledger + skills"):
  ACC-06 no longer refuses a bare resolve of a checked row, the check stays pending; ACC-15 moves
  the check out of `/kb-att` into `/kb-acc`; ACC-17's older-client clause follows ACC-06; A6 is
  rewritten. Each carries a dated `Superseded` paragraph quoting its old wording. ACC-20 (one
  answer, open or resolved) and ACC-21 (the CLI `attention check` verb and its teaching receipt)
  are appended at the end of the creation sequence with A19. ACC-09's web check-before-decision
  card is unchanged.
- 2026-09-29 — ACC-14 search-score evidence landed (`t-e9c0127a`):
  `search_scores_are_a_function_of_permitted_documents_only` plants an anchor task carrying a
  unique token once in a long body as a principal with board read and write on untagged rows,
  then adds a tag-denied document strongly matching a guessed prefix: the anchor's served
  `lexicalScore` and `score` are byte-identical with and without the denied row, which itself
  appears in neither receipt (A11). The landing fixed the leak as a bug; requirement wording
  is unchanged. `lexical_scores` normalised every bm25 by the strongest match on the whole
  board index, denied documents included, so a denied row moved a permitted hit's served
  scores — a prefix oracle for denied text. The permitted candidate set is now decided before
  any scoring; per-request tag-set memoization plus the event row joined into the document
  query remove the per-event second SELECT. Event documents are authorized as the event they
  index (same union the event tails filter on), since the task's tags alone handed a denied
  row to any caller who could read the task. HTTP proofs are moot: the web serving retired
  (`e-caeb1449`), so the evidence is CLI-process e2e. The recompute half: filtering only
  the divisor proved insufficient on re-review (M1): FTS5's per-row bm25 folds whole-index
  term frequencies, row count and average length into every strength, so under enforcement
  FTS now decides only which rows match while `permitted_bm25_scores` recomputes the
  strengths with N, df and avgdl over permitted rows only (same k1/b 1.2/0.75 and 8/2/4
  title/body/tags weights); `search_scores_and_order_are_a_function_of_permitted_documents_only`
  pins both hits' scores and their order. On unenforced boards the FTS5 strengths are kept
  unchanged, so scores and order are exactly what the unfiltered code produced.

- 2026-09-29 — task-attach evidence landed (`t-d2fd604a` port of `9626f1f`, CLI paths only):
  `note_attention_raise_and_sitrep_refuse_a_tag_denied_task_like_an_unknown_id` holds board
  read and write plus `visible` at both capabilities with no `secret` scope, then refuses a
  note, an `attention raise --task` and a `sitrep post --task` against a `secret` task with
  exit codes and stderr byte-identical to the same writes naming a never-created id (A21).
  The landing fixed the leak as a bug; requirement wording is unchanged. `add_note` checked
  board scope and `post_sitrep` checked nothing before trusting the task id, so a managed
  caller with board write but no read on the task's tag could attach rows to it — and success
  versus `not found` confirmed whether the row existed. Both now run `authorize_task_attach`
  under the write lock, and `raise_attention` runs it beside the new-row tag check it already
  held; an absent id answers `DeniedOrNotFound` under enforcement and keeps its plain
  `not found` where no guard can deny. A refusal writes nothing: each task-scoped ledger
  keeps only its birth event, and no attention row or sitrep is created. The remaining attach
  paths from the pre-serve fix (checkpoint, handoff, edges, subscriptions, deploy) are
  unchanged by this port and carry no new evidence here.

- 2026-09-29 — ACC-14 removed-task tail evidence landed (`t-bd66208d` port of `b7407a1`,
  CLI paths only): `a_removed_tasks_trail_stays_tag_gated_on_every_tail` tags a
  task `secret` after a body edit and then removes it: a caller holding only
  board read sees the pre-tag `previousBody` draft on no tail — board-wide
  `events`, a `watch --follow` stream, CLI `search` — and `events --task` on
  the gone row refuses exactly like a never-created id, while the same caller
  granted `secret` read keeps the trail on each of them (A22). The landing
  fixed the leak as a bug; requirement wording is unchanged. The tails
  authorized a removed task's events against the empty tag set of the missing
  row plus each event's own snapshot, so a pre-tag event stayed readable to
  any board reader, while the index dropped those same events under its
  stale-task rule. A removed task's events are now authorized against the
  task's last-known tags — the union of its `task_removed` snapshots — plus
  each event's own snapshot, in the tails and in the index alike; the served
  HTTP route halves of the pre-serve fix are moot with serve retired, and a
  removed task with no removal record still fails closed with the stale-entry
  tag.

- 2026-09-29 — ACC-14 removed-task-link evidence landed (`t-2cffbe08` port of
  `983265c`, CLI paths only): `removed_task_links_stay_tag_gated_on_every_listing_search_and_lane`
  seeds a sitrep, a handoff, an untagged attention row and a deployment attempt
  on a `secret` task plus genuinely taskless control rows, then removes the
  task: a caller holding only board read and write sees no trace of the
  orphaned rows in any listing, in search, in a lane-filtered sitrep listing
  (the stand-in for the retired `/api/v1/lanes` surface), on the deployment
  listing or in a `watch --follow` stream, and `handoff retire`, `handoff
  accept`, `attention show`, `deploy show` and `deploy finish` answer
  byte-identically to a never-created id — while every control row stays
  readable and the owner still sees every row (A23). The landing fixed the
  leak as a bug; requirement wording is unchanged apart from the new A23. The
  four links were `ON DELETE SET NULL`, so removing the task rewrote each
  row's `task_id` to NULL and every gate read the orphaned row as board
  scope. The board schema moves `35` → `36`: the links are plain `TEXT` with
  no foreign key — the `subscriptions.subject_task_id` precedent — so a
  removal keeps the id and the listings, the index (`document_row_tags`
  through the sibling cache's removal union), the handoff transitions and the
  deployment gates resolve it through the task's last-known removal tags,
  failing closed with no record. Rows already nulled by an older removal
  recover their link from their creation event when that event's task has a
  `task_removed` record and no live row; session handoffs, lanewide sitreps
  and taskless deployments stay NULL.

- 2026-09-29 — ACC-14 orphan follow-throughs landed (`t-2cffbe08` port of
  `b8ec141`, CLI paths only): accepting a handoff orphaned by removal is an
  acknowledgement that mints no lease (the accept receipt falls back to board
  scope rule summaries through `task_row_exists`, exactly like a session
  acknowledgement), and the archive sweep files orphaned handoffs and events
  away once they are old; a deployment naming a task with neither a live row
  nor a removal record hides just that row, and `doctor` reports it under
  `orphanedTaskLinks` — pinned by
  `an_orphaned_handoff_stays_deniable_yet_acceptable_and_archivable`,
  `compiled_binary_accepts_a_handoff_on_a_removed_task_and_archives_it`,
  `compiled_binary_hides_an_orphan_deployment_and_doctor_reports_it`,
  `store::tests::managed_deployment_listing_hides_an_orphan_link_and_doctor_reports_it`
  and `schema_36_backfills_pre_v36_nulled_links_from_creation_events` (A23).

- 2026-09-29 — ACC-14 id-reuse evidence landed (`t-2cffbe08` port of `776e77e`,
  CLI paths only):
  `removed_task_ids_are_never_reused_and_probe_like_live_denied_ids` removes
  a `secret` task carrying a sitrep and probes the freed id beside a live
  `secret` id as a caller holding board read and untagged write: both `task
  add --id` attempts answer the one generic denial with the same exit code
  and byte-identical stderr and record nothing, the sitrep reaches no
  listing, and the owner still reads it (A24).
  `reusing_a_task_id_is_refused_with_a_plain_message_where_no_guard_can_deny`
  pins the unenforced halves: a live id answers `task X already exists` and
  a removed id answers `task X was removed and its id cannot be reused`. The
  landing fixed the leak as a bug and added the A24 wording: `task add`
  refused a live id with the raw `UNIQUE constraint failed` — an existence
  oracle — while a removed id could be recreated, re-parenting every
  surviving linked row under the new row's tags. `refuse_reused_task_id` now
  refuses any id naming a live row, a `task_removed` record or any event
  history — read-gated under enforcement so a caller who cannot read the
  id's tags gets the generic denial — on `task add` and on import alike.

- 2026-09-25 — ACC-14 dependency-replacement evidence landed (`t-c718c024`):
  replacing a task's dependency list keeps the edges the caller cannot read
  instead of deleting them, so a writer on the row cannot silently remove a
  gate the owner set; the write still succeeds, because refusing would confirm
  a hidden edge exists. The receipt and the listing still withhold the kept
  edges while the gate keeps honouring every edge — pinned by
  `dependency_replacement_keeps_a_tag_denied_prerequisite` (A25). The parent
  edge needs no such treatment — the row's own `parentID` carries it, so a
  replacement drops an edge the caller already sees — and subscription
  relations are create-only, with each target gated at add and no rewrite
  path.

- 2026-09-25 — ACC-14 dependency-replacement follow-up (`t-c718c024`): the
  keep now covers every edge the caller cannot write, not just the unreadable
  ones — removing a gate is a write against the prerequisite's scope as much
  as the dependent's — and re-listing an edge already on the row needs no new
  authority, as with the unchanged parent. A readable but read-only kept edge
  stays visible in the listing as usual while the gate keeps honouring it;
  `--clear-dependencies` keeps the hidden and read-only edges the same way.
  Pinned by the extended `dependency_replacement_keeps_a_tag_denied_prerequisite`
  (A25): the owner gates `t-visible` on the unreadable `t-secret` and the
  read-only `t-ops`, and the caller keeps both across `--depends-on t-other`,
  a re-list of `t-ops`, and `--clear-dependencies`, while the owner can still
  drop any edge.

- 2026-09-25 — ACC-14 dependency-replacement oracle closed (`t-c718c024`
  review): the re-list skip in `update_task` applies only to existing edges
  the caller can read — re-listing the hidden `t-secret` is refused
  byte-identically to naming `t-nonexistent` (same exit code and stderr,
  nothing written), so guessing a hidden prerequisite confirms nothing,
  while re-listing the readable but read-only `t-ops` still needs no new
  authority and the kept loop still preserves the hidden edge. Pinned by the
  extended `dependency_replacement_keeps_a_tag_denied_prerequisite` (A25).

- 2026-09-25 — ACC-14 search-restriction evidence landed (`t-5f7daa1b`):
  `restricted_match_agrees_with_filter_afterwards_on_both_branches` now also
  covers a large mixed permitted set past the old 900-row chunk boundary
  (more than one old batch). The landing removes the `rowid IN` restriction
  it once pinned: a throwaway probe on the bundled SQLite 3.50.2 showed
  EXPLAIN QUERY PLAN printing a full `SCAN ... INDEX 0:=M1` for `MATCH ?
  AND rowid IN (...)` — FTS5 has no rowid seek to push a restriction into
  — and measured one MATCH execution per IN element (~2.7s for 2000
  permitted + 5000 denied, ~562ms for 500 permitted, against ~3–4.5ms for a
  single walk; `rowid = ?` per id and a VALUES join scaled the same way),
  so batching the restriction would run the denied-driven walk once per id
  instead of once per call. The probe was deleted afterwards.
  `fts_match_permitted` now runs exactly one rowid-only MATCH walk on every
  path with the permitted intersect in Rust: denied seqs are materialised as
  bare i64 rowids — MATCH time still grows with the denied rows holding the
  query's terms, one shared walk being the minimum FTS5 permits — while no
  denied content, snippet, or bm25 is ever read and the served strengths
  still come from the permitted-only recomputation. The three comments that
  said denied rows are "never materialised" now state exactly what is and is
  not materialised.

- 2026-09-25 — ACC-14 page-fill evidence landed (`t-b694bc83`):
  `managed_pages_fill_past_denied_rows_with_a_true_truncation_probe` seeds
  a `secret` attention row raised and settled on the readable `t-visible`
  plus newer-denied deployment attempts beside older visible ones: a caller
  holding board read and the `visible` tag gets a full page of visible rows
  from `events --task`, board-wide `events` and `deploy list` with a true
  `+1` truncation probe — while the owner still reads the denied rows first
  and granting `secret` restores them (A26). The landing fixed the leak as a bug; requirement wording is
  unchanged. The SQL `LIMIT` ran before the per-row tag test, so a denied
  row consumed a page slot and `events --task t-visible --limit 1` answered
  `[]` while visible history waited. The three listings now scan in chunks
  until the readable bound is filled, following the attention listing's
  filter-first pattern; unmanaged boards keep the SQL bound, byte-identical,
  and the unbound `current_deployments` is unchanged.

- 2026-09-25 — ACC-14 page-fill follow-up (`t-b694bc83` review): the chunked
  scans now page by keyset instead of LIMIT/OFFSET — `seq < last` newest-first
  and `seq > last` on the ascending tails, `(created_at, id)` on deployments —
  inside one read snapshot per page, so each chunk is an indexed seek and a
  write between chunks can neither duplicate nor skip a row. The
  cursor-driven tails (`events_since_filtered` and the watch poll) cap the raw rows examined per scan and advance the cursor past
  denied rows already read, so a long denied tail is walked a bounded page
  per scan instead of re-read whole on every revision; advancing past denied
  rows cannot skip a row that is visible under the authority and tags current
  at scan time, because the ascending cursor moves only over rows its own
  snapshot examined — a later grant or retag does not replay history already
  passed. Pinned by the extended
  `managed_pages_fill_past_denied_rows_with_a_true_truncation_probe` (three
  hundred denied events ahead of the visible history, with a five-row
  no-duplicates page) and the new
  `store::tests::managed_ascending_tail_walks_a_denied_stretch_in_bounded_scans`
  (A26).

- 2026-09-25 — ACC-14 tail-liveness follow-up (`t-b694bc83` review): the watch
  poll starts its static walk at the scan frontier instead of the cursor, so a
  `--limit 1` follower advances by the scan floor instead of one row per poll;
  a one-shot watch behind a denied stretch longer than the raw cap emits one
  `advanced` heartbeat carrying the frontier (ADR-031 already obliges
  consumers to persist it) instead of stalling its polling consumer, while an
  empty poll at the head stays silent — and off enforcement an empty one-shot
  stays silent too, since only a capped scan can hide a denied stretch worth
  walking. Pinned by
  `watch::tests::a_limit_1_follow_poll_advances_by_the_scan_floor_over_denied_rows`,
  `watch::tests::a_one_shot_watch_behind_denied_rows_reports_progress_not_silence`
  and `watch::tests::an_unenforced_one_shot_watch_stays_silent_behind_rejected_rows`
  (A26).

