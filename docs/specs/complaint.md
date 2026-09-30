# Specification: a lane raises a complaint through the existing attention machinery, as a sixth attention kind (slice COMPLAINT)

## 1. Identity and baseline

- **Slice ID:** `COMPLAINT`. Requirement IDs are `COMPLAINT-01` .. `COMPLAINT-07`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-23` at commit `7a837dbb9a60d9ad4c0bcaab9b0495cf939bcb92` on branch
  `wt-t-366502cb-spec-1790092721`. Every "today" claim below cites the line that
  has it, as `<absolute-path>:<line>`, read in that worktree. Board schema at the baseline is `32`
  (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2437`), and the sixth kind does not
  exist yet: the specification is written before the implementation, as ADR-047 §6 requires
  (per `/private/tmp/kanban-t-366502cb-spec-1790092721/docs/specs/README.md:15-22`).
- **Status:** `SPEC-READY` on 2026-09-23 (independent gate by a reviewer applying the SDD §1 exit criteria; OQ-1 in §7 is answered and gates nothing, so no material open question remains).
- This stamp authorises neither implementation nor rollout nor release; product readiness stays `/quality`, then `/tidy`, then the served-tier receipt.
- **Owner (product scope):** George. He alone resolves scope, whether a non-goal in §2 is
  reinstated, and the open questions in §7.
- **Decider (wording of this document):** the lane contract in kb task `t-366502cb` and slice row
  `t-40a8490a` under epic `e-c0852fe7`. Where this document and that contract differ on a
  fact, the contract wins and this document is corrected; where the contract is silent, the
  wording here is the decision. (These rows were taken from the delegating contract, not read
  directly: this worktree has no KB access. See the closing note in §7.)
- **Sources:**
  - **George, 2026-09-07:** "`we need a new feature: complaints`". This request is the slice
    approval ADR-047 §7 requires for a slice outside the `WEB`/`SPA` rollout, and `t-40a8490a`
    sits under epic `e-c0852fe7` for that reason (approval rule at
    `/private/tmp/kanban-t-366502cb-spec-1790092721/docs/specs/README.md:31-32`).
  - **George, 2026-09-14**, shape decision on attention card `a-fed2ebd3` (choice
    `attention-kind`): sixth attention kind `complaint` — kind enum plus validation plus docs
    plus e2e, no new table. The full attention machinery is reused end to end
    (raise/list/show/reply/reopen, events, search, generated MCP tools, JSON output, ADR-008
    named refusals); no new CLI verb is added.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §6 is why this slice is specified before implementation, §9 is
    why §6 carries no invented budget.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why an unknown
    kind is refused with a sentence that names every accepted value (the shape documented at
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:100-102`), and why a refusal
    writes nothing.
  - `docs/adr/ADR-010-adapters-generated-from-the-command-surface.md` — why the new kind value
    flows to adapters through the one surface table (`COMMANDS` at
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:871`, enum values at
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:1652`) rather than through
    hand-written tool schemas: the MCP tools are built from `COMMANDS`
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/mcp.rs:16`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/mcp.rs:233-236`).
  - `docs/adr/ADR-006-rust-runtime-and-compiled-binary-e2e.md` — why §8's mandatory evidence is a
    process-boundary exchange with the compiled binary rather than a library test.
  - `docs/adr/ADR-036-the-kb-skill-is-a-pinned-submodule-of-the-public-package.md` — why the
    `skills/kb` prose question is settled inside this document's §7 rather than by an edit here:
    `skills/kb` is a pinned submodule (`.gitmodules` carries a `[submodule "skills/kb"]`
    entry), and it is empty (uninitialised) in this worktree, so its prose was measured in the
    driver checkout instead (see OQ-1).
  - `docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md` — the decision
    card this slice reuses without changing: the resolution is derived by the composer at
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7053-7057`, never passed by the
    caller, and the row stays while only its state moves
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6942`).
  - Shipped surface at the baseline: `ATTENTION_KINDS` with exactly five values
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/model.rs:933`) beside `ATTENTION_STATUSES`
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/model.rs:937`); the kind CHECK in all
    four table declarations that carry one (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:164`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:750`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:1465`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2051`); `BOARD_MIGRATIONS`
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2971-2976`) and
    `BOARD_SCHEMA_VERSION` (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2437`); the
    CLI usage lines (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:206`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:216`), the `COMMANDS` attention
    rows (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:1301-1365`), the
    `ENUM_ARGUMENTS` attention rows (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:1695-1708`),
    and the `ATTENTION_FIELDS` key list
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:4213`); the shared validator
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:115-123`) with its value list
    renderer (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:103-112`); raise
    validation (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6420`), list
    validation (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6508-6509`),
    `Store::attention` (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6495`),
    `show_attention` (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6558`), the
    resolve ladder (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6943-7004`) and
    the reopen ladder (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7116-7136`);
    the `Attention` struct (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/model.rs:1735-1740`)
    and the `AttentionCard` projection
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/projection.rs:174-182`, served through
    `needs_you` at `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/projection.rs:325`); the
    web reply/reopen routes (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/serve.rs:698-699`,
    reopen at `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/serve.rs:812-824`, reply at
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/serve.rs:913-973`); the search arm
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:535-536`) and the event kinds
    (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:576`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6464`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7081`,
    `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7159`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section `## Requirements trace — docs/specs/complaint.md` is the trace
  of record when it lands (convention at
  `/private/tmp/kanban-t-366502cb-spec-1790092721/docs/testing/compiled-rust-e2e-matrix.md:207-210`).

## 2. Purpose and scope

**Intended outcome.** A lane can raise a complaint — something wrong that only the operator can
retire — through the exact attention machinery the other five kinds use, so a complaint is
raised, listed, answered, resolved and reopened like any other attention item and never needs
its own table, verbs, or adapters.

**Users / actors.**

- **Lane agents** — raise a complaint from the CLI (`attention raise TEXT --kind complaint`) and
  read it back on `attention list --kind complaint` and `attention show`.
- **The board operator (George)** — answers and settles complaints through `attention resolve`
  and the served reply form, and reopens a mistakenly settled one, exactly as for the other
  five kinds.
- **Adapter clients (MCP)** — read the generated tool schemas, which publish `complaint` as an
  accepted `--kind` value with no new tool (ADR-010).

**In scope.** The sixth `ATTENTION_KINDS` value; the `attention` kind CHECK in every table
declaration that carries one; the CLI usage text for `attention raise` and `attention list`;
the `BOARD_V33` migration; the refusal sentence for unknown kinds as re-rendered with six
values; process-boundary evidence for each mandatory requirement.

**Boundaries.**

- **The decision card** (ADR-042) is reused, not extended: choices, outcomes, the composer at
  `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7053-7057`, and the served reply
  form keep one behaviour for all six kinds.
- **Authorization** is untouched: raising stays open to any writer, resolving stays limited to
  the operator or the row's raiser
  (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6995-7004`), reopening to the
  operator or the resolver
  (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7131-7135`).
- **The `skills/kb` prose** needs no edit in this change: it names no kind enumeration (see
  OQ-1). It is owned by the pinned submodule (ADR-036), not by this slice.
- **The e2e matrix file** is updated by the landing change's trace section, not by this
  document: §8 is the draft the matrix copies.

**Non-goals.**

- **A new table, new CLI verbs, or new event kinds.** The decided shape reuses the attention
  row, the six attention verbs, and the `attention_raised` / `attention_resolved` /
  `attention_reopened` / `attention_updated` envelopes.
- **A product definition of what a complaint means.** The slice adds the kind value and its
  plumbing; what counts as a complaint is operator practice, not a documented rule.
- **Changing the resolve permission.** The raise/resolve asymmetry (any writer raises; only
  the operator or the raiser resolves) applies to complaints exactly as it does today.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

Refusal sentences below are quoted verbatim, with `{value}` as the substitution point for the
rejected kind. The list renderer that produces them is
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:103-112`, shared by every
`validate` call site including
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6420` and
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6509`.

### The sixth kind

**COMPLAINT-01** — Raise, list, and show a complaint through the existing attention verbs.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision.
`ATTENTION_KINDS` (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/model.rs:933`) gains
`complaint` appended after `risk`, becoming a six-value array. `attention raise TEXT --kind
complaint` stores a row whose `kind` reads back as `complaint` on `attention show` and on
`attention list --kind complaint`; `attention list` without a kind filter includes it alongside
the other five kinds. The usage lines at
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:206` and
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:216` name all six values.
Permissions: raising stays open to any writer, exactly as today.
Failure behaviour: none new — the existing raise refusals (empty body, unknown raiser, tag
rules) apply unchanged.
Data rules: the row is a normal attention row; `attention_raised` carries `"kind": "complaint"`
on the same payload shape as the other kinds
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6464-6466`).

**COMPLAINT-02** — Keep the existing five kinds working byte-identically.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision (no new table,
reuse the machinery).
Raising, listing, filtering, showing, answering, resolving and reopening `blocking`,
`decision`, `approval`, `review` and `risk` rows behave exactly as at the baseline: same flags,
same sentences, same JSON keys, same events. A board that only uses the five old kinds is
observably unchanged apart from the schema version bump that `COMPLAINT-04` requires.

**COMPLAINT-03** — Refuse an unknown kind with the sentence that names all six values.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision; ADR-008.
On `attention raise` and on `attention list --kind`, any `--kind {value}` outside the six is
refused with `invalid attention kind {value}; expected blocking, decision, approval, review,
risk, or complaint`, the output of the shared validator
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:115-123`) over the six-value
array in the order `COMPLAINT-01` fixes. (At the baseline the same function renders five
values: `invalid attention kind {value}; expected blocking, decision, approval, review, or
risk`.)
Failure behaviour: the command exits non-zero and writes nothing — no row, no event, no
migration side effect (ADR-008 fail-closed).

### Storage and migration

**COMPLAINT-04** — Migrate every existing attention row forward with zero data loss, re-runnably.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision (no new table).
`BOARD_V33` is appended to `BOARD_MIGRATIONS`
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2971-2976`) and
`BOARD_SCHEMA_VERSION` becomes `33`
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2437`). Opening a V32 board with
the compiled binary migrates it in place using the established table-rebuild pattern (the
`BOARD_V31` precedent at `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2039-2051`):
the rebuilt `attention` table carries the six-value kind CHECK, every existing row survives
with its kind, body, raiser, timestamps, status, decision card, check columns, tags and archive
state intact, and the `search_attention_*` triggers and attention indexes are recreated as
`BOARD_V31` recreates them
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2096-2110`).
Data rules: the migration writes the new schema and `user_version` only; it invents no rows and
edits no row content. The runner still refuses a newer-than-supported board with `database
version {current} is newer than supported version {}`
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2860-2864`).
Quality constraints: the ladder's last step survives a re-run against a board whose
`user_version` was lowered without reverting the schema, by naming every copied column — the
property `BOARD_V24`/`BOARD_V25` document at
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:1368-1371` and
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:1438-1442`.

### One behaviour for all six kinds

**COMPLAINT-05** — Resolve, reply, and reopen a complaint exactly like any other kind.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision; ADR-042.
`attention resolve`, the served reply form
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/serve.rs:913-973`), and `attention
reopen` treat a `complaint` row identically to the other five: the same decision composer
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7053-7057`), the same
`attention_resolved` / `attention_reopened` envelopes
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7081`,
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7159`), and the same
already-resolved refusal (`attention {id} was already resolved by {} — it is history, not a
queue entry` at `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6990-6993`).
Permissions: the raise/resolve asymmetry is unchanged — only the operator or the row's raiser
may resolve (`attention {id} was raised by {}; only geoyws or that same raiser may resolve it
— use attention update to correct it without closing George's queue` at
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:6999-7003`), and only the
operator or the resolver may reopen (`attention {id} was resolved by {}; only geoyws or that
resolver may reopen it` at
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/store.rs:7131-7135`).

**COMPLAINT-06** — Change no JSON shape apart from the new kind value.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision (validation
plus docs plus e2e, no new surface).
`Attention.kind` stays a plain string
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/model.rs:1740`); `attention list` rows
keep the `ATTENTION_FIELDS` keys
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:4213`); `AttentionCard` keeps its
four fields (`board`, `attention`, `body_html`, `task`)
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/projection.rs:176-182`); the
search document title keeps the `'attention: ' || kind` shape
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:535-536`); event payloads keep
their keys with `"kind": "complaint"` as the only new value. The served card renders no
kind-specific branch: the card carries the row and its typeset body, and the meta sentence
reads the task, not the kind
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/projection.rs:163-173`).

**COMPLAINT-07** — Publish the sixth value through the generated surface with no new tool.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-14 shape decision; ADR-010.
`schema --json` publishes `complaint` in the accepted `values` of `attention raise --kind`
and `attention list --kind` (the `ENUM_ARGUMENTS` rows at
`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:1695-1708` read the same
`ATTENTION_KINDS` array), and the MCP tool list gains no tool and renames none — the tools are
built from `COMMANDS`
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/mcp.rs:233-246`), whose attention rows
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:1301-1365`) keep their flags and
read-only marks.

## 4. Acceptance examples

### A1 (`COMPLAINT-01`, `COMPLAINT-05`, `COMPLAINT-06`)

*Given* an empty board at the migrated schema,
*when* a lane runs `attention raise "returns arrive broken" --as lane@driver-2 --kind complaint`,
then reads it with `attention show` and `attention list --kind complaint`, then the operator
resolves it with `attention resolve`,
*then* the raise prints a row with `"kind": "complaint"`; both reads return that row with the
same keys as any other kind; the resolve prints a settled row with `status resolved`; and the
event tail holds `attention_raised` then `attention_resolved`, both carrying
`"kind": "complaint"` on otherwise unchanged payload shapes.

### A2 (`COMPLAINT-03`)

*Given* a board at the migrated schema,
*when* a lane runs `attention raise "x" --as lane@driver-2 --kind grievance`,
*then* the command exits non-zero, prints `invalid attention kind grievance; expected blocking,
decision, approval, review, risk, or complaint`, and the board holds no new row and no new
event. The same sentence (with the offered value substituted) answers `attention list --kind
grievance`.

### A3 (`COMPLAINT-02`, `COMPLAINT-04`)

*Given* a board at schema 32 seeded with one open row of each of the five old kinds,
*when* the new compiled binary opens it and then raises one `complaint` row,
*then* the board reports schema 33; all five seeded rows survive with kind, body, raiser,
status and timestamps intact and still filterable by their own `--kind`; the complaint row
filters under `--kind complaint` only; and re-opening the migrated board migrates nothing
further and changes nothing.

### A4 (`COMPLAINT-05` resolve permission)

*Given* a board at the migrated schema with an open `complaint` row raised by
`lane@driver-2`,
*when* a third party who is neither the operator nor that raiser runs `attention resolve` on
it,
*then* the command exits non-zero with `attention {id} was raised by lane@driver-2; only
geoyws or that same raiser may resolve it — use attention update to correct it without
closing George's queue`, and the row stays open with no new event. The raiser resolving the
same row afterwards succeeds.

### A5 (`COMPLAINT-07`)

*Given* the compiled binary,
*when* a client reads `schema --json`,
*then* the `attention raise` and `attention list` operations publish `complaint` inside the
`--kind` accepted values, the tool (operation) names are exactly the baseline set, and no
operation gains or loses a flag.

Where the template asks for more categories: invalid input is A2; unauthorised access is A4;
failure recovery needs no scenario beyond A3 because the slice adds no new failure mode — a
migration step that fails leaves `user_version` unmoved, since the version bump commits only
with the step (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2903-2905`); idempotency
is the re-open half of A3; concurrency needs no scenario because the slice adds no write path
beyond the existing single-writer `IMMEDIATE` migration transaction
(`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2867`).

## 5. Contracts and data

- **Interface version or schema:** the CLI grammar gains one accepted `--kind` value on two
  operations (`attention raise`, `attention list`); usage text, `COMMANDS` flag lists and
  `ENUM_ARGUMENTS` shapes are otherwise unchanged. No command, flag, tool name, or read-only
  mark is added, removed, or renamed.
- **Data invariants:** every `attention` table declaration carries `kind TEXT NOT NULL
  CHECK(kind IN ('blocking','decision','approval','review','risk','complaint'))` in that order;
  the status CHECK (`open`, `resolved`) and every other column constraint are untouched. `kind`
  is written once at raise and never edited afterwards — `attention update` takes no `--kind`
  (its `COMMANDS` row at `/private/tmp/kanban-t-366502cb-spec-1790092721/rust/lib.rs:1334-1357`
  has none), so a row's kind is stable for its lifetime.
- **Migration:** forward only, V32 to V33, appended at the end of `BOARD_MIGRATIONS`; the
  table-rebuild pattern of the `BOARD_V31` precedent, naming every copied column so a re-run
  is safe. No backfill: existing rows keep their kinds, and no row becomes a complaint except
  by a fresh raise.
- **Compatibility:** a baseline (V32) binary opening a V33 board stops at the runner's guard:
  `database version 33 is newer than supported version 32`, and writes nothing. A V33 binary
  opening a V32 board migrates it forward on first touch. Reads are kind-agnostic (`kind` is a
  free string in `Attention` and in every projection), so mixed-version reads show the row
  while only the new binary can write the new value.
- **Ownership:** the board owns its attention rows; the operator owns the resolve/reopen
  decision on each one. This slice moves no ownership.

## 6. Quality and security

- **Reliability:** N/A — the slice adds no new failure mode: raise, resolve, reopen and the web
  reply route keep their existing refusals, and the migration runs inside the runner's
  per-step transaction.
- **Accessibility:** N/A — no served markup changes; the card renders from the same
  `AttentionCard` projection.
- **Privacy:** N/A — no new actor, identity, or visibility rule; tag-scoped checks run before
  kind validation exactly as today.
- **Security:** N/A — no new principal, permission, or trust boundary; the resolve/reopen
  ladders are untouched.
- **Operability:** a failed `BOARD_V33` step leaves the board at its prior `user_version`
  with prior data intact, because the version bump commits only with the step
  (`/private/tmp/kanban-t-366502cb-spec-1790092721/rust/db.rs:2903-2905`); the operator
  re-runs any ordinary command to retry, and a board already at V33 migrates nothing.
- **Performance:** observation only, not a budget — the migration is a one-time table rebuild
  of the same shape as `BOARD_V31`, and steady-state queries gain one more CHECK alternative
  on a write-only path; no timing commitment is made (see ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
:| --- | --- | --- | --- |
| OQ-1 | Does the `skills/kb` prose need a kind-list update for `complaint`? | George | answered 2026-09-23 | none — answered before implementation |

OQ-1 resolution: no update is needed. The skill names no kind enumeration anywhere; its only
kind mention is a single `--kind blocking` example (measured in the driver checkout at
`skills/kb/SKILL.md:261`, where the submodule is initialised — in this worktree `skills/kb`
is an empty uninitialised submodule, verified by directory listing plus the
`[submodule "skills/kb"]` entry in `.gitmodules`). An example value stays valid when the
accepted set grows, so there is no follow-up board row and no prose edit in this change. No
other material question remains.

Note on sources: the kb rows behind this document (`t-366502cb`, `t-40a8490a`, epic
`e-c0852fe7`, card `a-fed2ebd3`) and George's two quoted instructions were taken from the
delegating lane contract, not read directly — this worktree's constraints allow no KB access.
An independent reviewer with board access confirms the quotations against those rows.

## 8. Verification

Every test name below is planned, not existing: each is marked `planned` and no name is
claimed to exist at the baseline. Each is a compiled-binary process-boundary exchange (no
HTTP, no browser), landing in the `## Requirements trace —
docs/specs/complaint.md` matrix section on implementation.

| Requirement | Strength | Layer | Test name | Note |
:| --- | --- | --- | --- |
| `COMPLAINT-01` | `MUST` | `process` | planned: `complaint_kind_raise_list_show_round_trip` | `no e2e coverage` |
| `COMPLAINT-02` | `MUST` | `process` | planned: `five_legacy_kinds_unchanged_after_complaint_lands` | `no e2e coverage` |
| `COMPLAINT-03` | `MUST` | `process` | planned: `unknown_attention_kind_refusal_names_all_six` | asserts the A2 sentence; `no e2e coverage` |
| `COMPLAINT-04` | `MUST` | `process` | planned: `complaint_migration_carries_five_kind_board_forward` | seeds V32 five-kind board, migrates, re-opens; `no e2e coverage` |
| `COMPLAINT-05` | `MUST` | `process` | planned: `complaint_resolve_reopen_matches_other_kinds` | includes the A4 third-party refusal; `no e2e coverage` |
| `COMPLAINT-06` | `MUST` | `process` | planned: `complaint_json_keys_match_legacy_kind_shapes` | compares `--json` keys against a `blocking` row; `no e2e coverage` |
| `COMPLAINT-07` | `MUST` | `process` | planned: `generated_surface_publishes_complaint_without_new_tool` | reads `schema --json`; `no e2e coverage` |

No `chrome` evidence is planned: the slice changes no served markup, so there is no browser
surface to drive; the matrix rows say so plainly per the convention at
`/private/tmp/kanban-t-366502cb-spec-1790092721/docs/testing/compiled-rust-e2e-matrix.md:207-210`.

## 9. Change log

- `2026-09-23` — slice created at `COMPLAINT-01` .. `COMPLAINT-07`. No supersessions yet.
