# Specification: a decision card checks reusable system understanding before the decision (slice ACC)

## 1. Identity and baseline

- **Slice ID:** `ACC`. Requirement IDs are `ACC-01` .. `ACC-17`, stable across wording
  refinements; numbering is by creation and grouping is by topic.
- **Baseline:** `2026-09-21` at commit `04a66b1` on branch
  `docs/t-6a3dd1a6-acc-spec`.
- **Status:** `SPEC-READY` on 2026-09-21. An independent `/quality spec` review found five
  defects; all were closed, and George resolved its sole product blocker as **lock** on decision
  card `a-db9de88b`. Specification readiness authorises neither implementation, rollout nor release.
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
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 names the planned rows;
  this specification does not claim implementation evidence.

## 2. Purpose and scope

**Intended outcome.** Before George makes a durable decision, the decision card asks one
raiser-authored question that demonstrates the reusable system fact the decision depends on,
records the answer as data and never exposes its answer key to the browser in advance.

**Users / actors.**

- **Raiser** — authors and may update a native check while holding the relevant code,
  component, host or tier context.
- **George / decision maker** — answers the check through the web card, CLI or `/kb-att`, then
  makes the existing decision.
- **Clerk / other lane / MCP caller** — reads the permitted redacted projection but cannot
  synthesize, add or change a check.
- **Operator** — migrates legacy body-block ACCs and reads aggregate check results by subject.

**In scope.** The native check block on `attention raise` and `attention update`;
`attention resolve --check-answered`; its Store invariants and persisted result; list, show,
MCP and digest projections; `POST /attention/{project}/{id}/check`; the mounted decision card;
legacy `ACC:` body-block migration; and the `/kb` and `/kb-att` native-field cutover.

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

**ACC-06 — Require an answer when a checked row is resolved.**
Strength: MUST · Layer: process · Source: `e-5c8f7735` §Ledger; `t-d7e6af5f`.
`attention resolve ... --check-answered KEY` is required for a row carrying an unanswered check.
Omitting it refuses resolution and prints the check question; the row stays open and no decision,
check result or event is written. If the web has already recorded the one answer, later web or
CLI resolve finds it, asks nothing twice and requires no second `--check-answered`. Supplying the
flag for a row with no check is refused by name. A row with no check and no answer input resolves
exactly as before ACC.

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

**ACC-13 — Project the check with surface-specific redaction.**
Strength: MUST · Layer: process · Source: `e-5c8f7735` §Reads; `t-cbaff421` PERSISTENCE;
`t-94076221`.
For an authorized unanswered row, `att list --json`, MCP and the digest carry only the question,
choices and `about`; both explanation and answer are absent. The raiser's own
`att show`/`att show --json` may carry the complete definition. The browser projection obeys
ACC-10. After the answer is recorded, permitted reads may carry explanation, answer and the
stored result fields from ACC-07. Redaction removes fields rather than substituting a guessable
placeholder.

**ACC-14 — Inherit tenancy and tag visibility without an ACC bypass.**
Strength: MUST · Layer: http · Source: rollout brief; existing Store authorization boundary.
Every ACC read and write is authorized as the containing attention row is authorized. A caller
who cannot read the row receives the existing non-enumerating unauthorized/not-found behaviour
and no check metadata; a caller who can read it gets the appropriate ACC-13 projection. ACC adds
no cross-board, cross-tenant, tag or actor exception, and denial is indistinguishable for checked,
unchecked, known and unknown rows.

**ACC-15 — Cut readers and skills over to native data.**
Strength: MUST · Layer: process · Source: `e-bef5dd2a` root cause; `t-94076221`.
`lane-att.sh` and `/kb-att` read only the native check question, `about` and choices before
answer, ask the check first and pass the recorded answer to resolution.
`/kb` authors the five native inputs. The legacy body-block reader and clerk synthesis are
removed: a row with no native check is shown with no check and reported as drafting debt, never
silently given a generated one.

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
an older client cannot bypass a check because ACC-06 refuses checked-row resolution without the
answer.

## 4. Acceptance examples

### A1 — valid full block (`ACC-01`, `ACC-02`, `ACC-03`, `ACC-05`)

*Given* a raiser creates a carded attention row with a question of 160 characters ending `?`,
three in-bound choices, an answer naming one choice, a 400-character explanation naming the
component and `about=src/store.rs`,
*when* `attention raise` writes it,
*then* the raiser's full `att show --json` round-trips the definition and no recommendation or
outcome exists on the check or its choices.

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
*then* it is refused with the question, the row remains open and no decision, result or event is
written; *given* the web already recorded the answer, *when* web or CLI resolves without a second
answer flag, *then* resolution uses the recorded result, asks nothing twice and succeeds.

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

### A12 — reader redaction and raiser view (`ACC-13`)

*Given* an unanswered checked row, *when* authorized list JSON, MCP, digest and raiser's own show
are read, *then* list/MCP/digest carry only question, choices and `about`, while raiser's show
carries the full definition; neither list/MCP/digest explanation nor answer exists; *when* the
answer is recorded, *then* permitted reads may carry explanation, answer and stored result.

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

## 5. Contracts and data

- **Interface version or schema:** CLI adds the five definition inputs and
  `attention resolve --check-answered KEY`; MCP exposes their typed equivalents; HTTP adds
  `POST /attention/{project}/{id}/check` to the repository's pinned OpenAPI document. OpenAPI is
  applicable because this is an HTTP operation; it is not a substitute for CLI/MCP schemas.
- **Data invariants:** a definition is absent or complete; it contains question, two through four
  choices, one declared answer, explanation and `about`; result data contains
  `answered`/`correct`/`answeredAt` consistently or is absent; no check choice has recommendation
  or outcome semantics. Attempt data is prohibited. An answered open row's check definition and
  result are immutable until resolution; reopen atomically clears the current decision/result and
  restores the unanswered open-row state.
- **Migration:** schema bump at the ADR-045 boundary plus the one-shot, receipt-bearing conversion
  in ACC-16; forward migration only. Rows without a valid leading legacy block are unchanged.
- **Compatibility:** absent checks remain absent and preserve old read/resolve behaviour; checked
  rows fail closed against old resolvers that omit the answer; redaction is an omission, not a
  renamed or nullable answer field.
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
- **Privacy:** check contents inherit row tenancy and tag visibility. The answer is product-sensitive
  assessment data and is minimized per ACC-10/13; no new retention period is invented.
- **Security — ASVS applicability:** OWASP ASVS **5.0.0** is selected as verification guidance,
  not a certification claim. **V2.2.1** and **V2.2.2** apply to bounded/structural validation at a
  trusted service layer (ACC-01/02/04); **V8.3.1** applies because authorization must be enforced
  by the trusted Store rather than browser controls (ACC-05/14); **V14.2.6** applies because HTML,
  JSON and browser assets must disclose only the minimum data and never the pre-answer key
  (ACC-10/13). Authentication, cryptography and session-management chapters add no ACC-specific
  contract because this slice inherits the existing trusted-edge identity and creates no secret,
  credential or session mechanism.
- **Operability:** migration records one receipt per board; native result fields support reporting
  pass/miss/none by `about`; invalid legacy content is reported rather than half-written.
- **Performance:** no performance or latency target was approved, so none is specified.

## 7. Open questions

None.

**Resolved question history.** OQ-1 asked whether the raiser may rewrite an answered check while
the row remains open. George chose **lock** (the recommended choice) on decision card
`a-db9de88b` for `t-6a3dd1a6` on 2026-09-21: refuse the edit and preserve the definition,
recorded answer/result and unlocked choices until normal resolution. ACC-05 carries the contract.

## 8. Verification

The trace of record has one planned row for every mandatory ACC requirement. No test name, pass,
implementation or release is claimed before implementation.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `ACC-01` | MUST | process | `PLANNED` | full block plus partial/bounds/answer-key refusals |
| `ACC-02` | MUST | unit | `PLANNED` | explanation and all three `about` shapes/refusal |
| `ACC-03` | MUST | unit | `PLANNED` | no recommendation, outcome or decision mutation |
| `ACC-04` | MUST | unit | `PLANNED` | every agreed marker across all three text locations |
| `ACC-05` | MUST | process | `PLANNED` | raiser/other/geoyws, answered-open lock, resolved immutability and reopen-clear/restore paths |
| `ACC-06` | MUST | process | `PLANNED` | missing/no-check/undeclared-key paths plus web answer then web/CLI resolve without a second answer |
| `ACC-07` | MUST | process | `PLANNED` | persisted answer/correct/time and note agreement |
| `ACC-08` | MUST | process | `PLANNED` | right/wrong, immutable duplicate and serialized concurrent-answer paths |
| `ACC-09` | MUST | chrome | `PLANNED` | check-first, one answer, no retry, unlock and miss explanation |
| `ACC-10` | MUST | chrome | `PLANNED` | HTML/JSON/script/bundle sentinel sweep |
| `ACC-11` | MUST | http | `PLANNED` | shared POST, first-writer success and loser conflict/refusal |
| `ACC-12` | MUST | chrome | `PLANNED` | keyboard, pointer, focus, announcement and Undo |
| `ACC-13` | MUST | process | `PLANNED` | list/MCP/digest/show redaction by state and actor |
| `ACC-14` | MUST | http | `PLANNED` | non-enumerating tenancy/tag isolation |
| `ACC-15` | MUST | process | `PLANNED` | native digest/skills and no synthesis |
| `ACC-16` | MUST | process | `PLANNED` | schema migration, rerun and invalid legacy block |
| `ACC-17` | MUST | process | `PLANNED` | no-check older-client compatibility |

## 9. Change log

- 2026-09-21 — initial ACC specification drafted from the eight approved board sources; the later
  approved child rows' one-answer/no-retry contract supersedes the epic draft. Independent review
  removed an unapproved semantic classifier, applied later reader redaction and inherited write/
  lifecycle laws. George chose **lock** on `a-db9de88b`; ACC-05 now refuses answered-open edits,
  closing the sole open question and making the specification `SPEC-READY`.
