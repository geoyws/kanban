# Specification: `tag add` registers only namespaced tags, refused with the board's estate (slice CLI)

## 1. Identity and baseline

- **Slice ID:** `CLI`. Requirement IDs are `CLI-01` .. `CLI-05`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-25` at commit `57d26432e4e9aabed78792c44b990f66c6cdcc5c` on branch
  `wt/t-7f596f45-tagns`. Every "today" claim below cites the line that has it, as `<path>:<line>`.
- **Status:** `DRAFT` (written 2026-09-25 by the `t-7f596f45` lane writer before implementation,
  as ADR-047 §6 requires). George approved the slice, its implementation, its tests and the chip
  rendering landing in one change with matrix rows: attention `a-3990a3e3`, choice `approve`,
  2026-09-24. That approval is the slice admission ADR-047 §7 requires: `t-7f596f45` is the
  child row of rollout epic `e-c0852fe7` carrying the work (with `t-fb600b26` alongside it).
  Open question `OQ-1` in §7 is material and blocks `SPEC-READY`; no independent readiness
  gate has been run. This stamp authorises neither rollout nor release; product readiness
  stays `/quality`, then `/tidy`, then the served-tier receipt.
- **Owner (product scope):** George. He alone resolves scope, the estate map, and the refusal
  wording.
- **Decider (wording of this document):** the `t-7f596f45` lane writer, under the lane contract
  and George's `a-3990a3e3` approval.
- **Sources:**
  - George, 2026-09-24, attention `a-3990a3e3` (choice `approve`): "Spec delta, implementation,
    tests and chips land in one change with matrix rows". This is the slice approval; the chip
    rendering lands as a `WEB` delta (`WEB-60` in `docs/specs/web-ui.md`) in the same change.
  - Board rows `t-7f596f45` and `t-fb600b26` — the requirement source. The estate map below is
    recorded from that contract, not re-decided here: estates `ifca`, `unum`, `geoyws`; IFCA
    boards `px`, `fmx`, `hx`, `hrx`, `ix`, `mx-root`, `prjx-root`, `rentx-root`, `auditx-root`,
    `ifca-docs`, plus `prjx`; geoyws boards `kanban`, `omp`, `acies`, `dotfiles`, `geoyws`,
    `atmux`, `dash`, `gitea`, `journal`, `orch`, `hax`; Unum boards are those whose name starts
    with `unum`, plus `memberx`.
  - `docs/adr/ADR-015-tags-are-a-per-board-master-file.md:122-131` — the 2026-09-23 correction
    note: tags are `<estate>/<subsystem>`, bare subsystem spellings are historical, map rule
    `r-98ff7ad2`.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why the refusal
    below is a sentence that names the repair, and why it writes nothing.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §6 is why a changed CLI contract and changed refusal wording are
    specified before implementation, §7 is why the approval above admits a slice outside the
    `WEB`/`SPA` rollout.
  - Shipped surface at the baseline: `rust/store.rs:409` (`ESTATES`, the three registered
    estate names); `rust/store.rs:422-449` (`validate_tag_name`, which accepts slash-free
    names unchanged and refuses only slashed names under an unregistered estate);
    `rust/store.rs:6028-6060` (`Store::add_tag`, which registers any shaped name with no
    namespace check); `rust/lib.rs:189` (`kanban tag add NAME [--description TEXT] [--as ACTOR]
    [--json]`); `rust/lib.rs:7371-7381` (the `tag add` dispatch arm).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 names the rows this slice adds;
  this specification does not restate the matrix.

## 2. Purpose and scope

**Intended outcome.** A tag registered after this slice always says whose subsystem it names:
`kanban tag add` refuses a bare name with the sentence that builds the namespaced form from
the board's own estate, so the master file can never again grow a second vocabulary of
unowned subsystems beside the namespaced one.

**Users / actors.**

- George, alone, registering tags from a shell through `kb`/`kanban`.
- Agent lanes, registering the tag a new row needs through the CLI, MCP (which spawns the
  CLI), or `transact` batches (whose items run the same dispatch).

**In scope.** The `tag add` registration command and its refusal; the compile-time
board-to-estate map it reads; the unchanged `--tag` filter refusals it is held against.

**Boundaries.** The `--tag` filter paths (`task list`, `attention list`, rule task-tag
validation) are touched only as the behaviour that must not move (`CLI-05`) — they are owned
by ADR-015 and unchanged by this slice. The chip rendering is owned by slice `WEB`
(`WEB-60`, same change, bounded delta per `docs/specs/README.md` proportionality).

**Non-goals.**

- `tag rename` and `tag remove` keep their existing contracts: renaming migrates any
  registered spelling, including a bare legacy tag, onto a namespaced one. This slice refuses
  only new bare registrations.
- Cross-estate registration is not refused: `tag add unum/queuer` on an IFCA board succeeds
  today and still succeeds — board row `t-fb600b26`'s own title reads "refuse a name whose
  prefix is not a registered estate", so any registered estate prefix is accepted. The map
  suggests; it does not confine.
- No registry row, no board migration, no event kind. The map is compiled in beside
  `rust/store.rs:409` until a registry row supersedes it.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced state, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

Refusal sentences below are quoted verbatim, with `{name}` as the substitution point for the
rejected tag, `{estate}` for the board's mapped estate, and the estate list in the order
`rust/store.rs:409` fixes it (`ifca, unum, geoyws`). The board's estate is
`estate_for_board` over the addressed board's name — inside a batch, the batch's board
(an item argv carries no selector, so the batch board is the only addressed one); outside
a batch, the registry selection, falling back to the board's stored name. A board with no
stored name is unmapped.

### Registration

**CLI-01** — `tag add` refuses a bare name with the board's namespaced form.
Strength: `MUST` · Layer: `process` · Source: board rows `t-7f596f45`, `t-fb600b26`;
George `a-3990a3e3`; ADR-008.
On a board mapped to `{estate}`, `kanban tag add {name}` where `{name}` carries no `/` exits
non-zero and writes nothing — no master-file row, no event — with
`tag {name} is not namespaced: use {estate}/{name} (estates: ifca, unum, geoyws)`.
The shape check runs first: a malformed name keeps `validate_tag_name`'s existing sentence
(`rust/store.rs:433-438`), and a slashed name under an unregistered estate keeps its
existing sentence (`rust/store.rs:442-446`). `transact` items run the same dispatch against
the batch's board, so a batched bare registration fails the same way and the batch lands
nothing.
Permissions: registration stays open to any writer, exactly as today.

**CLI-02** — the board-to-estate map is compiled in beside `ESTATES`.
Strength: `MUST` · Layer: `unit` · Source: board rows `t-7f596f45`, `t-fb600b26` (decision
recorded, not re-decided here).
`estate_for_board` maps `px`, `fmx`, `hx`, `hrx`, `ix`, `mx-root`, `prjx-root`, `rentx-root`,
`auditx-root`, `ifca-docs` and `prjx` to `ifca`; `kanban`, `omp`, `acies`, `dotfiles`,
`geoyws`, `atmux`, `dash`, `gitea`, `journal`, `orch` and `hax` to `geoyws`; any name
starting with `unum`, and `memberx`, to `unum`; every other name to no estate. Adding a
fourth estate, or moving a board, is a deliberate edit to this map — never a side effect of
a mistyped tag. There is no second board list: `rust/store.rs:409` stays the one estate
vocabulary, and this map is the one board vocabulary beside it.

**CLI-03** — an unmapped board is refused with the estate list and no suggestion.
Strength: `MUST` · Layer: `process` · Source: board rows `t-7f596f45`, `t-fb600b26`;
George `a-3990a3e3`; ADR-008.
On a board `estate_for_board` maps to nothing, `kanban tag add {name}` where `{name}`
carries no `/` exits non-zero and writes nothing with
`tag {name} is not namespaced: no estate maps this board, so register it as
<estate>/{name} (estates: ifca, unum, geoyws)`.
No single `estate/name` form is suggested, because there is no board truth to build one
from; the estate list is the whole repair.

**CLI-04** — a namespaced name registers exactly as before.
Strength: `MUST` · Layer: `process` · Source: ADR-015; board rows `t-7f596f45`, `t-fb600b26`.
`kanban tag add {estate}/{subsystem}` where `{estate}` is one of `ESTATES`
(`rust/store.rs:409`: `ifca`, `unum`, `geoyws`), whatever the board's mapped estate,
succeeds, returns the master-file row, and the tag attaches exactly as a registered tag
does today (filter, list and rename remain ADR-015's). Data rules: one master-file row
plus one `tag_added` event, unchanged.

### The behaviour that must not move

**CLI-05** — `--tag` filters refuse unknown names exactly as today.
Strength: `MUST` · Layer: `process` · Source: ADR-015 (no retroactive suite: this pins the
touched edge rather than specifying filters).
`task list --tag {name}`, `attention list --tag {name}` and rule task-tag validation keep
their existing sentences for a name no master file holds — `tag {name} is not in this
board's master file{suggestion} — an unregistered tag would filter to nothing and read
like an answer` on the two listings, and `global rule task tag {name} is not
registered on any active board{suggestion}` on rules. A bare unknown name on a filter is
NOT rewritten to the `CLI-01`/`CLI-03` sentence: filters read the vocabulary, they do not
register into it.

## 4. Acceptance examples

### A1 (`CLI-01`)

*Given* board `prjx` (mapped `ifca`), board `kanban` (mapped `geoyws`), board `memberx` and
board `unum-ledger` (both mapped `unum`),
*when* `kanban tag add assistant` runs on each,
*then* each exits non-zero writing nothing (`tag list` stays empty and no `tag_added`
event is appended), with `not namespaced:
use ifca/assistant (estates: ifca, unum, geoyws)` on `prjx`, `not namespaced: use
geoyws/assistant (estates: ifca, unum, geoyws)` on `kanban`, and `not namespaced: use
unum/assistant (estates: ifca, unum, geoyws)` on `memberx` and `unum-ledger`.

### A2 (`CLI-03`)

*Given* a board no estate maps (no stored name matches `CLI-02`),
*when* `kanban tag add assistant` runs on it,
*then* it exits non-zero writing nothing, the refusal carries `(estates: ifca, unum,
geoyws)`, and it suggests no single `estate/assistant` form.

### A3 (`CLI-04`)

*Given* board `prjx`,
*when* `kanban tag add ifca/assistant` runs,
*then* it exits zero with the master-file row, and a task added with `--tag
ifca/assistant` reads it back.

### A4 (`CLI-05`)

*Given* a board whose master file holds no `nope`,
*when* `task list --tag nope`, `attention list --tag nope` and a rule add carrying `--tag
nope` each run,
*then* each exits non-zero with its baseline sentence (`is not in this board's master
file` with the `filter to nothing` clause on the two listings; `is not registered on any
active board` on the rule), and none carries `not namespaced`.

### A5 (`CLI-02`)

*Given* the compiled binary,
*when* the map is asked for `prjx`, `ix`, `kanban`, `hax`, `memberx`, `unum-ledger` and
`scratch`,
*then* it answers `ifca`, `ifca`, `geoyws`, `geoyws`, `unum`, `unum` and no estate.

### A6 (`CLI-01`)

*Given* board `prjx` (mapped `ifca`) and a second board `kanban` (mapped `geoyws`),
*when* `kanban transact --project prjx --items '[{"name":"tag_add","arguments":{"name":"assistant"}}]'`
runs from the `kanban` board's own checkout,
*then* the batch exits non-zero with `rolledBack: true`, the item error names
`not namespaced: use ifca/assistant` (never the cwd board's `geoyws/assistant`), and
`tag list` on `prjx` stays empty.

### A7 (`CLI-01`)

*Given* board `prjx`,
*when* `kanban tag add Assistant` and `kanban tag add ifac/assistant` each run,
*then* each exits non-zero with its baseline sentence — `tag Assistant is not a usable
name: lowercase letters, digits and inner hyphens only, so one concept cannot arrive
under two spellings` (`rust/store.rs:433-438`) and `tag ifac/assistant names estate
ifac, which is not registered: a namespaced tag is filed under one of ifca, unum,
geoyws, so one subsystem cannot arrive under two owners` (`rust/store.rs:442-446`) —
neither says `not namespaced`, and `tag list` stays empty.

Not exercised: unauthorised access (registration stays open to any writer, `CLI-01`),
concurrency and idempotency (unchanged `add_tag` behaviour at
`rust/store.rs:6028-6060`).

## 5. Contracts and data

- **Interface version or schema:** the CLI grammar is unchanged (`rust/lib.rs:189`):
  `kanban tag add NAME [--description TEXT] [--as ACTOR] [--json]`. Only the refusal set
  grows by the two sentences `CLI-01` and `CLI-03` quote. `--json` refusals keep the
  error-object shape (an object holding only `error`).
- **Data invariants:** a refused registration writes nothing: no `tags` row, no `tag_added`
  event, no registry touch. What is registered is what `validate_tag_name` shapes, as today.
- **Migration:** none. Bare legacy tags already registered stay registered, attachable and
  filterable; they migrate with `tag rename`, which is unchanged.
- **Compatibility:** an older caller offering a bare name to `tag add` is refused where it
  used to succeed — that is the slice. Every other caller spelling (namespaced add, attach,
  filter, rename, remove) behaves exactly as at the baseline.
- **Ownership:** each board owns its master file (ADR-015); the map is product vocabulary
  owned by George, compiled in beside `rust/store.rs:409`.

## 6. Quality and security

- **Reliability:** N/A — the refusal is a local string check before the write transaction's
  first statement; no new failure mode.
- **Accessibility:** N/A — CLI surface only; the chip half is `WEB-60`.
- **Privacy:** N/A — the refusal names the offered tag and the board's estate, both
  caller-supplied or caller-addressed.
- **Security:** the refusal is fail-closed per ADR-008 (non-zero exit, nothing written);
  no authorization posture changes — any writer may still register, exactly as today.
- **Operability:** the sentence is the repair: it names the exact command form to run.
- **Performance:** an observation, not a budget: the check is two string scans against a
  match table; no measurement is owed and none is claimed.

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | The attach refusal's repair `tag add {tag}` names a bare registration `CLI-01` now refuses; keep it (two-step repair) or build `{estate}/{tag}` from the same map? | George | open | `SPEC-READY` |

## 8. Verification

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `CLI-01` | MUST | process | `tag_add_refuses_a_bare_name_with_the_boards_mapped_estate`; `tag_add_in_transact_refuses_against_the_batch_board_not_the_cwd` | table-driven over `prjx`/`ifca`, `kanban`/`geoyws`, `memberx`/`unum`, `unum-ledger`/`unum`; asserts exit, sentence, an empty `tag list` and no `tag_added` event; the batch case runs `transact --project prjx` from the `kanban` checkout and asserts the `ifca` repair with `rolledBack: true`. no e2e coverage |
| `CLI-02` | MUST | unit | `estate_for_board_maps_each_named_board_to_its_estate` | in-process map proof over every named board plus the `unum*` rule and an unmapped name. no e2e coverage |
| `CLI-03` | MUST | process | `tag_add_refuses_a_bare_name_on_an_unmapped_board_with_the_estate_list_only` | asserts the estate list is carried and no single `estate/name` is suggested. no e2e coverage |
| `CLI-04` | MUST | process | `tag_add_registers_a_namespaced_tag` | `ifca/assistant` on `prjx`: registers, attaches, reads back. no e2e coverage |
| `CLI-05` | MUST | process | `tag_filters_refuse_unknown_names_exactly_as_before` | `task list`, `attention list` and rule task-tag validation refuse bare `nope` with their baseline sentences. no e2e coverage |

## 9. Change log

- `2026-09-25` — slice created with `CLI-01`..`CLI-05` (board rows `t-7f596f45`, `t-fb600b26`;
  approved by George in `a-3990a3e3` on 2026-09-24).
- `2026-09-25` — `/quality` spec review returned `BLOCKED`: `CLI-05` quoted the attach-path
  sentence for the listing filters (fixed to the filter-to-nothing sentence); added A6
  (batch board), A7 (shape/estate branches), the `OQ-1` attach-repair question, and the
  `e-c0852fe7` admission. Status stays `DRAFT`: `OQ-1` blocks `SPEC-READY`.
