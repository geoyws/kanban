# Specification: a task may name the models allowed to claim it, and a claim says which model it runs (slice MODEL)

## 1. Identity and baseline

- **Slice ID:** `MODEL`. Requirement IDs are `MODEL-01` .. `MODEL-18`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-19` at commit `7d1505d` on branch `docs/t-1331c416-model-spec`, a
  documentation worktree of `kanban-geoyws-driver`. Every "today" claim below cites the line that
  has it, as `<path>:<line>`, read in that worktree. Board schema at the baseline is `29`
  (`rust/db.rs:2294`), and none of the surface this slice specifies exists yet: the specification
  is written before the implementation, as ADR-047 §6 requires.
- **Status:** `SPEC-READY` on 2026-09-19 (independent gate by a reviewer applying the SDD §1 exit criteria over two rounds; two findings closed — A7 rewritten to the shipped handoff invocation, `task list --allowed-model` projected as a single value). It authorises neither implementation nor release.
  <!-- SPEC-READY is stamped here by an independent reviewer against the SDD reference's §1 exit
  criteria, not by the writer of this document. It authorises neither implementation nor rollout
  nor release. -->
- **Owner (product scope):** George. He alone resolves scope, whether a non-goal in §2 is
  reinstated, and the open questions in §7.
- **Decider (wording of this document):** the planner's `CONTRACT` in board row `t-1331c416`
  (epic `e-c0852fe7`), written 2026-09-19. Where this document and that contract differ on a
  fact, the contract wins and this document is corrected; where the contract is silent, the
  wording here is the decision.
- **Sources:**
  - **George, 2026-09-19**, in the kanban planner pane: "add a new feature called model
    restricted — only a subset of models, or one model, can claim this task. Blender must only be
    used by Astra." This request is the slice approval ADR-047 §7 requires for a slice outside the
    `WEB`/`SPA` rollout, and `t-1331c416` sits under epic `e-c0852fe7` for that reason.
  - Board row `t-1331c416` — the planner's contract: the schema, the name shape, the wire fields,
    the CLI flags, the enforcement order, the refusal sentences, the events and the test set. Its
    numbered items are cited below as `t-1331c416 §<n>`.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §7 is why this slice may exist at all, §9 is why §6 carries no
    invented budget.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why each refusal
    below names the flag that fixes it, why an unreadable model name is refused rather than
    normalised, and why `--allowed-model` with `--clear-allowed-models` is refused rather than
    ranked.
  - `docs/adr/ADR-010-adapters-generated-from-the-command-surface.md` — why the new flags are
    declared in the one surface table (`rust/lib.rs:1001`-`rust/lib.rs:1193`) rather than parsed
    ad hoc: the MCP tool schemas are generated from it.
  - `docs/adr/ADR-006-rust-runtime-and-compiled-binary-e2e.md` — why §8's mandatory evidence is a
    process-boundary exchange with the compiled binary rather than a library test.
  - `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` — the chain the
    `task_claimed`, `handoff_accepted` and `task_updated` payloads below join.
  - `docs/adr/ADR-027-rules-are-one-tag-scoped-kb-document.md` and
    `docs/adr/ADR-015-tags-are-a-per-board-master-file.md` — the two mechanisms this slice
    deliberately is not (see §2 Boundaries): a rule frames work on claim and never refuses one, a
    tag says what a row is about and `--tag` on `claim --candidates` is a caller-side filter.
  - `docs/adr/ADR-036-the-kb-skill-is-a-pinned-submodule-of-the-public-package.md` — why the
    `skills/kb` prose is a follow-up row and not an edit in this change (OQ-2).
  - `docs/adr/ADR-049-model-restricted-tasks.md` — the decision record for this slice, written by
    the implementer in the same change (`t-1331c416 §10`). It is **pending** at this baseline: it
    does not exist in this worktree, and this document states the contract at requirement level
    without depending on it.
  - Shipped surface at the baseline: `rust/store.rs:5058` (`Store::claim`),
    `rust/store.rs:5110`-`rust/store.rs:5135` (the refusal ladder: claimable type, draft ancestor,
    status, blocking gates, live lease, `driver_only`, `assignee`), `rust/store.rs:5138` (the
    `task_claims` INSERT), `rust/store.rs:5152`-`rust/store.rs:5169` (the `task_claimed` event and
    its conditional `sprintOverride` key), `rust/store.rs:2608`-`rust/store.rs:2618` (`routable`
    in `eligible_claim_candidates`), `rust/store.rs:6596`-`rust/store.rs:6613` (the
    `accept_handoff` ladder), `rust/store.rs:6615` (its `task_claims` INSERT),
    `rust/store.rs:6628`-`rust/store.rs:6642` (the `handoff_accepted` event),
    `rust/store.rs:4991`-`rust/store.rs:5004` (`update_task`'s changed-fields list),
    `rust/store.rs:443` (`attach_tags`, one query for a whole list),
    `rust/store.rs:2346`-`rust/store.rs:2353` (`UpdateTask.tags`, `None` leaves alone and `Some`
    replaces wholesale), `rust/store.rs:2354` (`AcceptHandoffOptions`), `rust/store.rs:2363`
    (`ClaimOptions`), `rust/model.rs:480` (`Task`), `rust/model.rs:527` (`Claim`),
    `rust/lib.rs:1001`-`rust/lib.rs:1193` (the surface table: `task add` at `:1001`, `task list`
    at `:1025`, `task update` at `:1057`, `claim` at `:1096`, `handoff accept` at `:1180`),
    `rust/lib.rs:340` (the known-flag list), `rust/lib.rs:1134` and `rust/lib.rs:1156`
    (`checkpoint --model` and `handoff create --model`, the free-text model fields that already
    ship), `rust/lib.rs:4099` (the `--fields` key list), `rust/lib.rs:6800`-`rust/lib.rs:6812`
    (the mutually-exclusive pair table and its refusal sentence), `rust/db.rs:2750`
    (`BOARD_MIGRATIONS`, ending at `BOARD_V29`), `rust/db.rs:2294` (`BOARD_SCHEMA_VERSION`, `29`),
    `rust/db.rs:1368`-`rust/db.rs:1442` (the `BOARD_V24`/`BOARD_V25` doc comments on surviving a
    re-run), `rust/serve.rs:3762` (`facts`, the task-detail `<dl>`), `rust/serve.rs:3661`-
    `rust/serve.rs:3675` (the `Held by` block), `README.md:76`-`README.md:82` (what
    `claim --candidates` excludes).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section `## Requirements trace — docs/specs/model-restriction.md` is the trace
  of record and the two are kept byte-identical by the same change.

## 2. Purpose and scope

**Intended outcome.** A task can say which models may claim it, and a claim says which model it
runs as, so work that only one model can finish — George's example is Blender work that only Astra
can do — is refused to every other model at the moment of claiming instead of burning a lane that
discovers it cannot finish.

**Users / actors.**

- **The board operator (George)** — files and edits the allow-list from the CLI
  (`task add --allowed-model`, `task update --allowed-model | --clear-allowed-models`), and reads
  it back on `task show`, `task list --allowed-model` and the served task page.
- **Lane agents and their harness** — pass `--model` on `claim`, `claim --next`,
  `claim --candidates` and `handoff accept`, and receive the refusal (naming the allowed set) when
  the row is not theirs to take. The harness knows which model it is running; nothing infers it.
- **Adapter clients (MCP)** — read the generated tool schemas, which gain an allowed-model array
  on the task-writing tools and a model string on `claim` and `handoff accept` (ADR-010).

**In scope.**

- The task-side allow-list: the `allowedModels` wire field on `Task`, `task add --allowed-model`,
  `task update --allowed-model | --clear-allowed-models`, `task list --allowed-model`, and the
  `allowedModels` entry in the `task_updated` changed-fields list.
- The claim-side declaration: `claim --model` (honoured by `--next` and `--candidates`),
  `handoff accept --model`, the `model` field on `Claim` and `ClaimSummary`, and the `model` key
  on the `task_claimed` and `handoff_accepted` payloads.
- The refusal itself, on all three paths — `Store::claim` (`rust/store.rs:5058`), `accept_handoff`
  (`rust/store.rs:6596`) and candidate routing (`rust/store.rs:2608`) — with the exact sentences
  in `MODEL-09`, `MODEL-10` and `MODEL-13`.
- The model-name shape, validated identically on the task side and the claim side.
- The `BOARD_V30` migration (`task_models`, its index, and `task_claims.model`) as it runs on a
  real V29 board, including re-running the ladder's last step.
- The two projections: the generated MCP/`schema --json` surface, and the served task detail page's
  `allowed models` row and holder `model` line.

**Boundaries.**

- **Authorization** is the store's and is untouched. An allow-list is a scheduling constraint, not
  a permission: a caller who may write the board may edit any row's allow-list, and the tag-scoped
  read/write checks (`rust/store.rs:5103`) decide visibility exactly as they do today.
- **Rules** (ADR-027's `ONLY:`/`EXCEPT:` selectors) frame work and are injected on claim; they
  never refuse one. This slice owns the refusal, and adds no selector to the rule grammar.
- **Tags** (ADR-015) stay a per-board master file describing what a row is about; `--tag` on
  `claim --candidates` stays a caller-side filter. The allow-list is neither, and no tag gains
  meaning here.
- **Sprint boundary** (ADR-045 §3) keeps its own refusal and its own place in the ladder; this
  slice inserts one check and moves none.
- **The `skills/kb` prose** is a pinned submodule (ADR-036) and is changed by the follow-up row in
  OQ-2, not by this change.
- **The served page's design system** is ADR-046's, preserved by citation: the new row reuses
  `row(label, value)` (`rust/serve.rs:3795`) inside the existing `<dl>`, and adds no style.

**Non-goals.** `t-1331c416`'s closing paragraph names these as deliberately out of scope, and this
specification does not add them:

- **A model registry.** Model names are free text validated by shape only, like the `--model`
  that already ships on `checkpoint` and `handoff create` (`rust/lib.rs:1134`, `rust/lib.rs:1156`).
  A typo in an allow-list fails closed — nobody can claim the row — and the refusal prints the set,
  so the typo is visible rather than silent.
- **Changing `checkpoint --model` / `handoff create --model` semantics.** Those fields stay free
  text recorded for the reader; this slice neither reads nor validates them.
- **Per-lane default models.** Nothing infers a model from the lane, the actor or the environment:
  the harness knows what it runs and passes it.
- **Agreement between a claim's `--model` and later checkpoints' `--model`.** No check compares
  them, and a mismatch is not an error.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

Refusal sentences below are quoted verbatim from `t-1331c416 §6` and `§2`, with `{id}`, `{m}` and
`[A, B]` as substitution points; `[A, B]` renders the row's allowed set in sorted order, comma and
space separated, inside square brackets. A refusal is an exit-non-zero refusal that writes nothing
(ADR-008).

### Model names

**MODEL-01** — Validate a model name by shape, on both sides, before anything is written.
Strength: `MUST` · Layer: `unit` · Source: `t-1331c416 §2`.
A model name is accepted only when it matches `^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$`: it begins with
an ASCII letter or digit, continues with letters, digits, `.`, `_`, `:`, `/` or `-`, and is 1 to 64
characters long. Matching is exact and case-sensitive everywhere the name is compared, so `Astra`
and `astra` are two different models.
Permissions: none — the shape is checked for every caller on every path.
Failure behaviour: any other value is refused, on `task add --allowed-model`,
`task update --allowed-model`, `claim --model` and `handoff accept --model` alike, with
`model name {m} is not a usable name: it must match ^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$`. The
refusal leaves the board unchanged: no allow-list row is written, no claim is taken, no event is
appended.
Data rules: names are stored exactly as given — never trimmed, lowercased or otherwise normalised.

### The task allow-list

**MODEL-02** — Carry the allow-list on every task, always present and always sorted.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §3`.
`Task` gains `allowed_models: Vec<String>`, serialised as `allowedModels`: a JSON array of strings
present on every task in every projection that serialises a `Task`, sorted ascending by byte order,
with no duplicates. An empty array means unrestricted and is the value every task carries today; a
board that never uses the feature therefore differs from a board at the baseline by exactly one
always-empty array key.
Data rules: the list is attached in one query per read, the way `attach_tags` does
(`rust/store.rs:443`), never one query per row; duplicates collapse on write, so passing the same
name twice stores one row.

**MODEL-03** — Set the allow-list when the task is filed.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §5`.
`task add` accepts `--allowed-model NAME`, repeatable; the names are validated (`MODEL-01`) before
the task row is inserted, and the resulting task reads back with exactly those names in
`allowedModels`. Omitting the flag files an unrestricted task with an empty array.
Failure behaviour: one bad name refuses the whole command and no task is created.

**MODEL-04** — Replace or clear the allow-list on an existing task.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §3`, `§5`.
`task update --allowed-model NAME` (repeatable) replaces the whole list wholesale, exactly as
`--tag` replaces tags (`rust/store.rs:2350`); `task update --clear-allowed-models` empties it,
returning the row to unrestricted. Passing neither flag leaves the list untouched, including when
other fields of the same row change.
Data rules: replacement is a delete-then-insert inside the update's transaction, so a refused
update leaves the previous list intact.

**MODEL-05** — Refuse the two update flags together.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §5`; ADR-008.
`--allowed-model` and `--clear-allowed-models` join the mutually-exclusive pair table at
`rust/lib.rs:6800`, so passing both is refused with that table's sentence,
`--allowed-model and --clear-allowed-models are mutually exclusive`, before the store is opened for
writing.
Failure behaviour: nothing is written — not the allow-list, not the other fields of the same
`task update` invocation, and no `task_updated` event.

**MODEL-06** — Filter a listing by allowed model, server-side.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §5`.
`task list --allowed-model NAME` returns exactly the rows whose allow-list contains `NAME` under
the same exact, case-sensitive comparison as `MODEL-01`. Unrestricted rows never match, because an
empty list contains nothing. The filter is applied in the query, not by the caller, and composes
with the listing's existing filters and its ADR-037 bound.

**MODEL-07** — Name the allow-list among the fields an update moved.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §7`.
When an update changes the set of allowed models, `allowedModels` appears in the changed-fields
list of the `task_updated` payload (`rust/store.rs:4991`-`rust/store.rs:5004`). When the update
leaves the set as it was — including `--allowed-model` naming the same names in a different order,
and `--clear-allowed-models` on an already-empty list — `allowedModels` does not appear, so the
ledger records a move only where one happened.

### The claim declaration

**MODEL-08** — Record on the claim the model it declared.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §4`, `§6`.
`claim --model NAME` and `handoff accept --model NAME` validate the name (`MODEL-01`) and write it
to the new `task_claims.model` column in the same INSERT that creates the lease
(`rust/store.rs:5138`, `rust/store.rs:6615`). `Claim` and `ClaimSummary` gain
`model: Option<String>`, serialised `model`: the declared name, or `null` when the caller passed no
`--model`. A claim taken without `--model` on an unrestricted row is accepted and reads
`"model": null`.
Data rules: the column is nullable and never back-filled; the model of a released or expired claim
is whatever that claim declared.

**MODEL-09** — Refuse a claim on a restricted task that declares no model.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §6`.
When a task's allow-list is non-empty and the caller passed no `--model`, `claim <id>` is refused
with `task {id} is restricted to models [A, B]; pass --model with one of them to claim it`, naming
the row's whole sorted set so the caller can see what would work.
Failure behaviour: no lease row, no `task_claimed` event, the task's status unchanged.

**MODEL-10** — Refuse a claim whose model is not on the list.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §6`.
When a task's allow-list is non-empty and the caller passed `--model m` with `m` not in the list
(exact, case-sensitive), `claim <id>` is refused with
`task {id} is restricted to models [A, B]; model {m} may not claim it`. When `m` is in the list the
claim proceeds through the rest of the ladder normally, and `MODEL-08` records it.
Failure behaviour: as `MODEL-09` — nothing written.

**MODEL-11** — Keep the refusal ladder in one order on the direct claim path.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §6`.
On `Store::claim` the model check sits immediately after the driver-only check and immediately
before the assignee check, so the order a caller experiences is: claimable type → draft ancestor →
status → blocking gates → live lease → driver-only → **model** → assignee
(`rust/store.rs:5110`-`rust/store.rs:5135`). A driver-only row that is also model-restricted refuses
a non-driver caller as driver-only, not as model-restricted; a model-restricted row assigned to
someone else refuses a wrong-model caller as model-restricted, not as assigned.

**MODEL-12** — Route restricted rows to a matching model only.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §6`.
In `eligible_claim_candidates` (`rust/store.rs:2608`) a restricted candidate is routable only when
the caller passed `--model` and that model is in the candidate's list; otherwise the row is absent
from `claim --candidates` and is never selected by `claim --next`. Unrestricted candidates are
unaffected by `--model`: passing one neither adds nor removes them. A restricted row skipped this
way produces no refusal — the pool is simply smaller — while naming the row explicitly still
refuses with `MODEL-09` or `MODEL-10`, so a caller learns why.
Quality constraints: `claim --candidates` stays the read-only scheduler view of ADR-026 — this
filter reads and writes nothing.

**MODEL-13** — Honour the allow-list at handoff acceptance, in the same words.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §6`.
`handoff accept --model` runs the same two checks as `MODEL-09` and `MODEL-10` against the
handoff's task, with byte-identical sentences, placed immediately after the driver-only check at
`rust/store.rs:6596`. An accepted handoff records its model on the claim row it creates
(`MODEL-08`); a refused one leaves the handoff `pending`, creates no claim and appends no event.

**MODEL-14** — Record the declared model on the ledger, only when there was one.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §6`.
When `--model m` was given, the `task_claimed` and `handoff_accepted` payloads carry `"model": m`;
when it was not, the payload has no `model` key at all, so a board that never uses the feature
writes byte-identical events to the baseline. This is the conditional-payload pattern
`sprintOverride` already uses (`rust/store.rs:5160`, `rust/store.rs:6635`), and the payloads stay on
the one ADR-029 hash chain.

### Storage and migration

**MODEL-15** — Migrate a V29 board to carry the allow-list and the claim's model.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §1`.
`BOARD_V30` is appended to `BOARD_MIGRATIONS` (`rust/db.rs:2750`) and `BOARD_SCHEMA_VERSION` becomes
`30` (`rust/db.rs:2294`). Opening a real V29 board with the compiled binary migrates it in place:
the board gains `task_models(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, model
TEXT NOT NULL, PRIMARY KEY(task_id, model)) STRICT`, the index `idx_task_models_model`, and the
column `task_claims.model TEXT`. Every existing task reads back `"allowedModels": []` and every
existing claim reads back `"model": null`; no row is rewritten and no data is lost.
Data rules: the step survives being re-run against a board whose `user_version` was lowered without
reverting the schema — the condition
`compiled_binary_still_migrates_a_board_that_is_behind` creates — so the table is created with
`CREATE TABLE IF NOT EXISTS` and the column is added only when it is absent, the property
`BOARD_V24` and `BOARD_V25` state in their doc comments (`rust/db.rs:1368`-`rust/db.rs:1442`). See
OQ-1: those two steps obtain it by rebuilding a table with every column named, which a bare
`ALTER TABLE ADD COLUMN` cannot do.

**MODEL-16** — Keep the allow-list consistent with its task.
Strength: `MUST` · Layer: `none` · Source: `t-1331c416 §1`, `§3`.
The allow-list is durable and belongs to its task: it survives closing and reopening the board,
the primary key makes a repeated `(task_id, model)` pair impossible, and removing the task removes
its allow-list rows through `ON DELETE CASCADE` rather than leaving orphans behind. The property
is structural — it is the table's primary key and its foreign key — and no test in this slice
fails when it breaks; §8 says so plainly and OQ-4 owns the gap.

### Projections

**MODEL-17** — Publish the new flags in the generated surface.
Strength: `MUST` · Layer: `process` · Source: `t-1331c416 §5`; ADR-010.
`allowed-model` is declared in the surface table (`rust/lib.rs:1001`-`rust/lib.rs:1193`) as a
repeatable flag on `task add` and `task update`, as a **single-valued** flag on `task list` — one
name is asked about, not a set — and `model` as a single-valued flag on `claim` and
`handoff accept`, with `clear-allowed-models` as a boolean on `task update`. `schema --json`
therefore publishes kind `list` for `allowed-model` on `task add` and `task update`, kind
`boolean` for `clear-allowed-models`, and kind `value` for `allowed-model` on `task list` and for
`model` on `claim` and `handoff accept`; the generated MCP tool schemas follow (ADR-010), so
`task_add` and `task_update` expose `allowed-model` as an array of strings while `task_list`
exposes it as a string, and `claim` and `handoff_accept` expose `model` as a string.
`allowedModels` is accepted in the `--fields` key list (`rust/lib.rs:4099`), the new flags appear
in the known-flag list (`rust/lib.rs:340`), and `transact`'s `claim` and `handoff_accept` items
accept `model` through the same table without separate parsing.

**MODEL-18** — Show the restriction and the holder's model on the served task page.
Strength: `MUST` · Layer: `unit` · Source: `t-1331c416 §9`.
The task detail page's facts `<dl>` (`rust/serve.rs:3762`) carries a row labelled `allowed models`
listing the row's sorted set when it is non-empty, and carries no such row when the task is
unrestricted; the `Held by` block (`rust/serve.rs:3661`-`rust/serve.rs:3675`) carries a `model` row
when the live claim declared one, and omits it when it did not. Both reuse the existing
`row(label, value)` helper and its escaping, so the values are HTML-escaped and the page gains no
new style.
Quality constraints: layout-only within ADR-046's system — no new component, no design pass.

## 4. Acceptance examples

Every invocation below is written as it runs against the compiled binary, with `--as` on every
write, and is executable as written once the implementation lands.

### A1 (`MODEL-03`, `MODEL-02`, `MODEL-09`)

*Given* a board where `kb task add "Rig the turntable" --allowed-model Astra --as @:geoyws/kanban/driver --json`
filed task `T`,
*when* a lane runs `kb claim T --as @:geoyws/kanban/be --json` with no `--model`,
*then* the command exits non-zero with
`task T is restricted to models [Astra]; pass --model with one of them to claim it`, `kb task show T --json`
still reads `"allowedModels": ["Astra"]` with no claim, and `kb events --task T --json` holds no
`task_claimed` event.

### A2 (`MODEL-10`, `MODEL-08`, `MODEL-14`)

*Given* the same task `T` restricted to `Astra`,
*when* a lane runs `kb claim T --model Blender --as @:geoyws/kanban/be --json`, and then
`kb claim T --model Astra --as @:geoyws/kanban/be --json`,
*then* the first exits non-zero with `task T is restricted to models [Astra]; model Blender may not claim it`
and writes nothing, the second succeeds and its claim reads `"model": "Astra"`, and the
`task_claimed` event for it carries `"model": "Astra"`.

### A3 (`MODEL-08`, `MODEL-14`)

*Given* an unrestricted task `U` (`kb task add "Sweep the logs" --as @:geoyws/kanban/driver --json`),
*when* a lane runs `kb claim U --as @:geoyws/kanban/be --json` with no `--model`,
*then* the claim succeeds, reads `"model": null`, and its `task_claimed` payload has no `model`
key — the same bytes a board at the baseline wrote.

### A4 (`MODEL-12`)

*Given* a restricted task `T` (`Astra`) and an unrestricted task `U`, both `todo` and both otherwise
claimable,
*when* a lane runs `kb claim --candidates --as @:geoyws/kanban/be --json`, then the same with
`--model Blender`, then the same with `--model Astra`,
*then* the first two list `U` and not `T`, the third lists both, and
`kb claim --next --model Astra --as @:geoyws/kanban/be --json` may select `T`, while
`kb claim --next --model Blender --as @:geoyws/kanban/be --json` never does.

### A5 (`MODEL-04`, `MODEL-07`, `MODEL-02`)

*Given* task `T` restricted to `Astra`,
*when* the operator runs `kb task update T --allowed-model Blender --allowed-model Astra --as @:geoyws/kanban/driver --json`
and then `kb task update T --clear-allowed-models --as @:geoyws/kanban/driver --json`,
*then* after the first `allowedModels` reads `["Astra", "Blender"]` — sorted, replacing the
previous set — and its `task_updated` payload names `allowedModels` among the changed fields; after
the second `allowedModels` reads `[]`, its `task_updated` payload names `allowedModels` again, and
`kb claim T --as @:geoyws/kanban/be --json` now succeeds with no `--model`.

### A6 (`MODEL-05`, `MODEL-01`)

*Given* task `T` restricted to `Astra`,
*when* the operator runs
`kb task update T --allowed-model Astra --clear-allowed-models --as @:geoyws/kanban/driver --json`,
and separately `kb task update T --allowed-model "big model" --as @:geoyws/kanban/driver --json`,
*then* the first is refused with `--allowed-model and --clear-allowed-models are mutually exclusive`
and the second with
`model name big model is not a usable name: it must match ^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$`; after
both, `allowedModels` still reads `["Astra"]` and no `task_updated` event was appended.

### A7 (`MODEL-13`)

*Given* task `T` restricted to `Astra`, claimed by the outgoing lane with
`kb claim T --model Astra --as @:geoyws/kanban/driver --json` whose receipt carries lease token
`TOKEN`, which then files a handoff with
`kb handoff create T --lease TOKEN --as @:geoyws/kanban/driver --summary "half rendered" --intent "finish the render" --next-action "render frames 40-90" --reason session_end --to @:geoyws/kanban/be --json`,
returning handoff `H`,
*when* the receiving lane runs `kb handoff accept H --as @:geoyws/kanban/be --json`, then
`kb handoff accept H --model Kimi --as @:geoyws/kanban/be --json`, then
`kb handoff accept H --model Astra --as @:geoyws/kanban/be --json`,
*then* the first is refused with
`task T is restricted to models [Astra]; pass --model with one of them to claim it` and the second
with `task T is restricted to models [Astra]; model Kimi may not claim it`, the handoff stays
`pending` through both, and the third is accepted with the new claim reading `"model": "Astra"`
and the `handoff_accepted` event in `kb events --task T --json` carrying `"model": "Astra"`.

### A8 (`MODEL-11`)

*Given* a task `D` that is both driver-only and restricted to `Astra`, and a task `Z` restricted to
`Astra` and assigned to `@:geoyws/kanban/driver`,
*when* a non-driver lane runs `kb claim D --as @:geoyws/kanban/be --json` and another lane runs
`kb claim Z --model Blender --as @:geoyws/kanban/be --json`,
*then* the first is refused with `task D is driver-only` (driver-only is checked first) and the
second with `task Z is restricted to models [Astra]; model Blender may not claim it` (the model
check precedes the assignee check).

### A9 (`MODEL-15`)

*Given* a board file written at schema 29 with a task and a live claim on it,
*when* the compiled binary at schema 30 opens it and the operator runs `kb task show <id> --json`
and reads the claim,
*then* the board carries `task_models`, `idx_task_models_model` and `task_claims.model`, the task
reads `"allowedModels": []`, the pre-existing claim reads `"model": null`, and re-running the
migration ladder's last step against the already-migrated board succeeds rather than failing on a
duplicate table or column.

### A10 (`MODEL-06`, `MODEL-17`, `MODEL-18`)

*Given* task `T` restricted to `Astra` and unrestricted task `U`,
*when* the operator runs `kb task list --allowed-model Astra --json`, reads `kb schema --json`, and
loads the served page `/task/{project}/T`,
*then* the listing holds `T` and not `U`; the schema publishes `allowed-model` as kind `list` on
`task add` and `task update` and kind `value` on `task list`, and `model` as kind `value` on
`claim` and `handoff accept` — so the MCP tools `task_add` and `task_update` type `allowed-model`
as an array of strings, `task_list` types it as a string, and `claim` and `handoff_accept` type
`model` as a string; and the page shows an `allowed models` row reading `Astra` plus — while the
lane holds it with `--model Astra` — a `model` row in `Held by`.

### A11 (`MODEL-16`)

*Given* task `T` filed with `kb task add "Rig the turntable" --allowed-model Astra --allowed-model Astra --as @:geoyws/kanban/driver --json`,
*when* the operator reads `kb task show T --json`, closes and reopens the board with a second
invocation of the same command, and then runs
`kb task remove T --as @:geoyws/kanban/driver --force --json`,
*then* both reads show `"allowedModels": ["Astra"]` — one entry, not two — and after the removal
the board holds no `task_models` row for `T`.

## 5. Contracts and data

- **Interface version or schema:** board schema `BOARD_SCHEMA_VERSION` moves `29` → `30`
  (`rust/db.rs:2294`) with `BOARD_V30` appended to `BOARD_MIGRATIONS` (`rust/db.rs:2750`). CLI
  grammar gains `task add --allowed-model NAME` (repeatable),
  `task update --allowed-model NAME | --clear-allowed-models`, `task list --allowed-model NAME`,
  `claim --model NAME` and `handoff accept --model NAME`, all declared in the one surface table so
  the MCP schemas follow (ADR-010). Wire shape: `Task.allowedModels` is a sorted string array
  always present; `Claim.model` and `ClaimSummary.model` are nullable strings; event payloads
  `task_claimed` and `handoff_accepted` gain an optional `model` key and `task_updated` gains
  `allowedModels` in its changed-fields list. Event kinds: none added.
- **Data invariants:** `task_models(task_id, model)` is the primary key, so a task lists a model at
  most once; every `task_models.model` matches `MODEL-01`'s shape; every `task_models.task_id`
  references a live task and is removed with it; `allowedModels` is always sorted ascending on the
  wire regardless of insertion order; `task_claims.model` is either `NULL` or a name matching
  `MODEL-01`; an empty allow-list and the absence of one are the same state — there is no third,
  "unset", value.
- **Migration:** forward-only, `BOARD_V30`: `CREATE TABLE IF NOT EXISTS task_models(...) STRICT`,
  `CREATE INDEX IF NOT EXISTS idx_task_models_model ON task_models(model)`, and
  `ALTER TABLE task_claims ADD COLUMN model TEXT` applied only when the column is absent. There is
  no down migration; the project has never shipped one (`rust/db.rs:2750` is an append-only ladder)
  and this slice does not introduce the concept.
- **Compatibility:** a board written before this slice migrates in place and reads as unrestricted:
  tasks report `[]`, claims report `null`. A client older than this slice ignores the new keys;
  a board that never sets an allow-list emits events byte-identical to the baseline (`MODEL-14`).
  A binary older than this slice cannot open a V30 board — the ladder is forward-only, as it has
  been since `BOARD_V1`.
- **Ownership:** the board owns the data. The allow-list is a task field, written by whoever may
  write the task; the claim's model is written once by the claiming agent and never edited
  afterwards.

## 6. Quality and security

- **Reliability:** every refusal in §3 is inside the claim's or the update's transaction, so a
  refused operation leaves no lease, no allow-list row and no event (`MODEL-05`, `MODEL-09`,
  `MODEL-10`, `MODEL-13`). A typo in an allow-list fails closed — no model can claim the row — and
  the refusal prints the whole set, so the typo is visible at the first attempt rather than
  silently routing work to the wrong lane.
- **Accessibility:** the served rows are `<dt>`/`<dd>` pairs in the page's existing definition
  list, carrying text, no colour-only meaning and no new interactive control (`MODEL-18`).
- **Privacy:** a model name is a scheduling label, not personal data; nothing new is logged beyond
  the name the caller passed, and only where the caller passed it (`MODEL-14`).
- **Security:** the allow-list is **not** an authorization control and must not be read as one. It
  refuses a claim; it does not restrict who may read a row, who may edit the list, or who may move
  the task. Existing tag-scoped authorization is untouched. There is no registry and therefore no
  registry to poison; names are matched exactly and case-sensitively, never normalised or
  fuzzy-matched, so `Astra` never satisfies a list holding `astra`.
- **Operability:** the refusal names the row, the allowed set and the flag that fixes it, so an
  operator reading a lane's log can act without opening the board (ADR-008). `task list
  --allowed-model NAME` answers "what is waiting for this model" in one command, and
  `idx_task_models_model` is what makes that a lookup rather than a scan.
- **Performance:** no budget is set (ADR-047 §9). Two structural facts, as observations only: the
  allow-list is attached with one query per read, not one per row (`MODEL-02`, the `attach_tags`
  pattern at `rust/store.rs:443`), and `idx_task_models_model` exists so `MODEL-06`'s filter is an
  index lookup. Neither is a measured number and neither is a commitment.

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | `t-1331c416 §1` asks for re-run safety "the way `BOARD_V24`/`BOARD_V25` doc comments describe", but those steps obtain it by rebuilding a table with every column named (`rust/db.rs:1425`-`rust/db.rs:1442`), which a bare `ALTER TABLE task_claims ADD COLUMN model TEXT` cannot do, and SQLite has no `ADD COLUMN IF NOT EXISTS`. Is the intended mechanism a column-presence check before the `ALTER`, or a `task_claims` rebuild? | George | open | `/quality` on the implementation, not this specification's gate: `MODEL-15` states the observable property (the step survives a re-run) and either mechanism satisfies it |
| OQ-2 | The `skills/kb` prose ("Working a task" gains `--model`; the refusals listed under "Refusals worth knowing") lives in a pinned submodule (ADR-036) and cannot be edited in this change. Who files and lands the `geoyws/kb-skill` PR and the gitlink bump? | George | open | `RESOLVE-WHEN` on `t-1331c416`; not the spec gate |
| OQ-3 | A model registry is deliberately absent (§2 Non-goals), so a typo restricts a row to a model that does not exist. Revisit only if that bites in practice — what is the trigger to revisit? | George | open | nothing; recorded so the non-goal is a decision with an owner rather than an omission |
| OQ-4 | `MODEL-16`'s durability, dedupe and cascade rules have no test in this slice's fixed test set (confirmed with the implementer, 2026-09-19), so the row lands as `none` / `no e2e coverage`. Who files the row that adds the three assertions named in §8? | George | open | nothing in this slice; the coverage gap is declared rather than implied |

## 8. Verification

The rows below name the tests the **same commit** lands: at this baseline none of them exists yet,
because the specification is written before the implementation (ADR-047 §6). Each name is the one
fixed by `t-1331c416`'s contract and assigned to the implementer, and the main loop verifies every
name against `cargo test -- --list` before the `SPEC-READY` stamp — a row whose test is not
enumerated there is a lie and blocks the stamp (ADR-047 §5). `Layer` is the specification's own
vocabulary: `process` is a compiled-binary process-boundary exchange in `tests/e2e.rs`, `unit` is an
in-process `#[test]` in `rust/model.rs` or `rust/serve.rs`. Of the 18 requirements — all `MUST` —
15 are proved at `process` and 2 at `unit`, and 1 carries `none` and says `no e2e coverage`
plainly: `MODEL-16`, whose durability and cascade rules this slice's tests do not assert. There is
no browser evidence in this slice at all, and the one served-markup requirement says so in its
Note.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `MODEL-01` | MUST | unit | `a_model_name_has_one_shape` | `rust/model.rs` `mod tests`: the regex table — accepted and refused shapes, the 64-character bound, the leading-character rule, and case sensitivity |
| `MODEL-02` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | reads `allowedModels` back off `task show --json`: present on every task, sorted, empty on an unrestricted row |
| `MODEL-03` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | `task add --allowed-model` repeated, then read back as the filed row's list |
| `MODEL-04` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | the wholesale replace and the `--clear-allowed-models` empty, each read back |
| `MODEL-05` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | both flags together refused with the pair sentence, and a bad name refused, with the previous list intact |
| `MODEL-06` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | `task list --allowed-model` returns the restricted row and not the unrestricted one |
| `MODEL-07` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | `allowedModels` in the `task_updated` changed-fields list when the set moved, absent when it did not |
| `MODEL-08` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | the allowed claim succeeds and `claim.model` reads the declared name; an unrestricted claim without `--model` reads `null` |
| `MODEL-09` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | the verbatim `pass --model with one of them to claim it` refusal, with the sorted set rendered, and no claim written |
| `MODEL-10` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | the verbatim `model {m} may not claim it` refusal, then the allowed model succeeding |
| `MODEL-11` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | a driver-only restricted row refuses as driver-only, and an assigned restricted row refuses as model-restricted — the order assertion |
| `MODEL-12` | MUST | process | `claim_next_and_candidates_skip_restricted_rows_unless_the_model_matches` | the restricted row absent from `--candidates` with no model and with a non-listed one, present with a matching one, and `--next` never selecting it otherwise; unrestricted rows unaffected |
| `MODEL-13` | MUST | process | `handoff_accept_honours_the_task_model_allow_list` | acceptance refused without a matching `--model` with the identical sentence and the handoff still `pending`, then accepted with it |
| `MODEL-14` | MUST | process | `handoff_accept_honours_the_task_model_allow_list` | `model` on the `handoff_accepted` payload when given; `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` holds the `task_claimed` half and the absent key |
| `MODEL-15` | MUST | process | `a_v29_board_gains_task_models_and_claim_model_and_existing_claims_read_null` | a real V29 board opens at 30, gains the table, index and column, and its pre-existing claim reads `model: null`; `compiled_binary_still_migrates_a_board_that_is_behind` holds the re-run half of the same step |
| `MODEL-16` | MUST | none | `none` | no e2e coverage — owed, owner George (OQ-4): reopen a board and re-read a restricted row's list, pass the same `--allowed-model` twice and read one entry back, and remove the task and assert its `task_models` rows are gone. The property is structural today — the primary key and the `ON DELETE CASCADE` on `task_models` |
| `MODEL-17` | MUST | process | `mcp_schema_exposes_allowed_model_array_and_claim_model_string` | `schema --json` kinds `list`/`list`/`value` for `allowed-model` on `task add`/`task update`/`task list` and `value` for `model`; the MCP tools type it array on `task_add`/`task_update`, string on `task_list`, and `model` string on `claim`/`handoff accept` |
| `MODEL-18` | MUST | unit | `task_detail_lists_allowed_models_and_the_holders_model_unit` | `rust/serve.rs` `mod tests` reading served bytes: the `allowed models` row present when restricted and absent when not, and the holder's `model` line — no e2e coverage, and none planned for this row |

## 9. Change log

- `2026-09-19` — slice created at `MODEL-01` .. `MODEL-18`. No supersessions yet.
