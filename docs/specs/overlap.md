# Specification: duplicate filings are refused by deterministic title and path rules while hybrid top-k candidates wait for human acknowledgement (slice OVERLAP)

## 1. Identity and baseline

- **Slice ID:** `OVERLAP`. Requirement IDs are `OV-01` .. `OV-12`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-29` at commit `92c7686b268db93298c237e1eadcb3fc98d71080` on branch
  `wt/t-3ea99369-spec`. Every "today" claim below cites the line that
  has it, as `<path>:<line>`, read at the baseline commit in
  `/Users/geoyws/work/wt/kanban-t-3ea99369-spec-8c1d/`. Board schema at the baseline is `36`
  (`rust/db.rs:2920`), and no overlap surface exists yet: no `touches` field on any row, no
  `--overlap-ack` flag, no `overlap` subcommand in the `COMMANDS` table
  (`rust/lib.rs:124`-`rust/lib.rs:292`), and no deterministic duplicate rule anywhere beside the
  import id-overlap refusal (`rust/import.rs:462`-`rust/import.rs:472`). The specification is
  written before the implementation, as ADR-047 §6 requires (per
  `docs/specs/README.md:15-21`).
- **Status:** `PROPOSAL` (not `SPEC-READY`: readiness needs the independent review against the
  SDD §1 exit criteria, which main arranges — the writer does not self-stamp; it authorises
  neither implementation nor rollout nor release).
- **Owner (product scope):** George. He alone resolves scope, the normalisation and `touches`
  open questions in §7, and whether a non-goal in §2 is reinstated. He planned this slice on
  2026-09-28 (planner pane, `e-e7aa716a`); no explicit slice row under epic `e-c0852fe7` is
  known to the writer — this worktree's constraints allow no KB access — and it is recorded
  here as an open approval pointer (OQ-3), not filed by this document.
- **Decider (wording of this document):** OverlapSpec, the writer of this slice. Where this
  document and the delegating contract differ on a fact, the contract wins and this document is
  corrected (see the closing note in §7).
- **Sources:**
  - **Eval verdict DROP Laya** (kb `t-91e669a8` note 560; full evidence
    `/Users/geoyws/work/wt/laya-eval-t-91e669a8/EVAL.md`): on corpus-v1 (7 duplicate / 220
    distinct pairs) the zero-false-refusal bar is hybrid 1/7 against Laya 0/7 on every signal
    and both state constructions; hybrid retrieval recall@1 0/7, @3 4/7, @5 5/7, @10 7/7;
    per-filing latency hybrid ~0.1–0.6s against Laya ~40s for k=5. Consequence: no judge
    process, no semantic threshold, no model revision to pin.
  - **George gates** (via the delegating contract): corpus-first `a-9438f17d` (satisfied by the
    corpus plus the eval above); refuse stays deterministic (normalised-title rule plus
    touches/file-path rules); hybrid top-k feeds `--overlap-ack` triage (warn, never refuse).
  - *Superseded 2026-09-29 (this verdict):* the note-449 judge-process-model plan — a Laya
    judge process with a pinned model revision behind the filing path — is `VOID`. The eval
    margins leave it nothing to contribute: it refuses none of the 7 duplicates without also
    refusing distinct rows, retrieves nothing the hybrid retriever does not already surface,
    and costs ~40s per filing against ~0.1–0.6s. The pinned revision
    (`receptron/laya-onnx@68f27df`, `laya.onnx a874eb25..6dba1e`) is recorded in the eval,
    not in this slice, and no `ort` build-time download and no 1.7 GB bundle enter the
    product.
  - `docs/adr/ADR-054-overlap-uses-deterministic-refuse-with-hybrid-triage.md` — the decision
    this specification implements.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §6 is why this slice is specified before implementation, §9 is
    why §6 carries an observation beside the budget.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why a refused
    write exits non-zero and writes nothing, with a sentence that names the blocking row.
  - `docs/adr/ADR-023-sqlite-native-hybrid-rag-search.md` — the hybrid retriever (`exact_score`
    in `rust/search.rs:484`, score components in `rust/model.rs:2446`-`rust/model.rs:2450`)
    whose top-k this slice surfaces for triage without changing.
  - `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` — why the
    acknowledgement in `OV-08` is a chained audit event.
  - `docs/adr/ADR-053-the-web-view-is-retired.md` — why every surface below is CLI: the serve
    layer is deleted with no replacement UI, so no web panel is specified, no served-bytes
    evidence is owed, and every `Layer` below is `process`.
  - Shipped surface at the baseline: `BOARD_SCHEMA_VERSION` and `BOARD_MIGRATIONS`
    (`rust/db.rs:2920`, `rust/db.rs:3479`-`rust/db.rs:3482`); the `task add` grammar
    (`rust/lib.rs:125`-`rust/lib.rs:128`) and the `task update` grammar
    (`rust/lib.rs:142`-`rust/lib.rs:145`); the `attention raise` grammar
    (`rust/lib.rs:202`-`rust/lib.rs:204`); the `claim [ID | --next]` grammar
    (`rust/lib.rs:152`-`rust/lib.rs:155`); the tag-authorization non-enumerating denial
    (`rust/authz.rs:34`); the import id-overlap refusal precedent
    (`rust/import.rs:462`-`rust/import.rs:472`); the claim-sweep read path
    (`rust/store.rs:3536`-`rust/store.rs:3538`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section `## Requirements trace — docs/specs/overlap.md` is the trace
  of record when it lands (convention at
  `docs/testing/compiled-rust-e2e-matrix.md:224-236`).

## 2. Purpose and scope

**Intended outcome.** The same work is filed once: an exact re-filing is refused with a
sentence naming the row it duplicates, a filing that walks the same files is refused the same
way, and everything merely similar is shown to the filer as scored candidates to acknowledge —
never refused by a score.

**Users / actors.**

- **Lane agents** — file tasks, raise attention rows, and claim work from the CLI; they read
  warn payloads and acknowledge the candidates they have considered.
- **The board operator (George)** — owns the normalisation and `touches` open questions in
  §7 and reads sweep output when the board looks doubled.
- **Adapter clients (MCP)** — reach the same verbs through the generated surface; no new trust
  boundary is added.

**In scope.** The normalised-title refuse rule; the `touches` path/glob refuse rule; the
`--overlap-ack` acknowledgement with its audit event; the hybrid top-k warn payload; the
cross-board claim provenance check; the `overlap sweep` command; the per-write budget. All
CLI: `serve` is retired (ADR-053).

**Boundaries.**

- **The hybrid retriever is read, not owned.** Scoring (`rust/search.rs:484`,
  `rust/model.rs:2446`-`rust/model.rs:2450`) is ADR-023's; this slice pins only the query
  construction (the new filing's title), the top-k depth, and the payload shape. A change to
  scoring is ADR-023's delta, not this slice's.
- **Tag authorization is reused, not restated.** Every verb below checks the caller's readable
  and writable tag scope as the containing board checks it today, and an unreadable row
  answers the existing non-enumerating denial (`rust/authz.rs:34`). No requirement below
  weakens that: a candidate the caller may not read is absent from the payload, not redacted
  in it.
- **The CLI grammar is pinned in §3, not guessed later.** New flags (`--touches`,
  `--overlap-ack`) follow the existing `COMMANDS` conventions (`rust/lib.rs:124`-
  `rust/lib.rs:292`): `--as` naming the acting caller on every write, `--json` on every
  verb. The sentences the product says are `OV-01`/`OV-02` regardless of argv.

**Non-goals.**

- **A judge process.** No model, no pair-judge step, no `ort` dependency, no embedding bundle,
  no pinned revision anywhere in the product. The eval's verdict (DROP Laya, §1) closes this;
  a future judge arrives as a new slice with its own corpus evidence, never as a drift of
  this one.
- **A semantic refuse threshold.** No score — hybrid or otherwise — refuses a write. The eval
  measured that none exists (semantic neg max 0.612 exceeds 6/7 positives;
  `/Users/geoyws/work/wt/laya-eval-t-91e669a8/EVAL.md:54-58`); scores only order the warn
  payload (`OV-07`).
- **A definition of sameness beyond the two rules.** What counts as "the same work" for this
  slice is exactly the normalised-title rule (`OV-01`) and the touches path/glob rule
  (`OV-02`). Anything subtler is a candidate for acknowledgement, not a refusal.
- **Retroactive deduplication on landing.** `overlap sweep` reports; it merges, closes, or
  rewrites nothing. A merge verb is a later slice if George asks for one.
- **A web panel.** `serve` is retired under ADR-053; there is no surface to add it to.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that will prove the requirement:
`process` throughout — the slice ships no pages and no browser surface at all after the
2026-09-29 retirement, so compiled-binary process-boundary exchanges prove what the slice
states.

Refusal sentences below are quoted verbatim, with `{id}` and `{value}` as the substitution
points. Every refusal exits non-zero and writes nothing — no row, no acknowledgement, no
event, no migration side effect (ADR-008 fail-closed). The CLI argv below is the pinned
contract; the sentences are what the product says regardless of the argv that reaches them.

### Deterministic refuse

**OV-01** — Refuse a filing whose normalised title matches an open row.
Strength: `MUST` · Layer: `process` · Source: George gate (refuse stays deterministic,
normalised-title rule).
Normalisation is exactly: lowercase ASCII A–Z; trim leading and trailing whitespace; collapse
each interior run of whitespace to one space. Two titles that agree after that mapping are
the same title. A checked write (`OV-06`) whose normalised title equals the normalised title
of an open row (any status but `done`, `cancelled`, or archived) on the same board is refused
with `duplicate of {id}: this filing has the same title after normalisation`, naming the
lowest open duplicate id, and writes nothing. Punctuation, non-ASCII folding, stop-words, and
stemming are NOT part of the mapping — they are OQ-1. The comparison MUST NOT consult any
score: title equality after the mapping refuses on its own, and title inequality after the
mapping never refuses, however high a hybrid score reads (the eval's zero-false-refusal bar:
hybrid exact-tier `score >= 1.0` is the only clean numeric rule at 1/7 recall, so the
specification refuses on equality, not on threshold).

**OV-02** — Refuse a filing whose touched paths collide with an open row's.
Strength: `MUST` · Layer: `process` · Source: George gate (touches/file-path rules).
A `touches` entry is a repo-relative POSIX path prefix or a glob in the board's repo
vocabulary (`*`, `**`, `?`, character classes; the repo identity the paths resolve against
is OQ-2). Two entries collide when their path sets intersect after expansion: an exact
path collides with itself, a prefix collides with everything beneath it, and a glob
collides with everything it matches. Exact paths and prefixes compare as strings with no
tree read, so their behaviour is identical under every tree below. A glob expands against
exactly one expansion tree, chosen by precedence: (1) when the filing carries cross-board
provenance (`OV-10`), the repo tree at the provenance commit; (2) otherwise — `task add`,
`attention raise`, and same-board `claim`, which carry no provenance — the filing lane's
checked-out repo tree at write time, i.e. the working tree including uncommitted files
the lane can see. A checked write (`OV-06`) carrying at least one `touches` entry that
collides with a `touches` entry of an open row on the same board is refused with `touches
overlap with {id}: {value}`, where `{value}` is the colliding entry as the filer wrote
it, and writes nothing. Rows with no `touches` entries never collide under this rule;
emptiness is not agreement.

**OV-03** — Catch the four known `--lane` duplicates by rule, not by score.
Strength: `MUST` · Layer: `process` · Source: eval verdict (the four `--lane` duplicates
`t-14e6feb7 <- t-403f6e88`/`t-1fce7662`/`t-9eafe212`, hybrid ranks 3/3/6 at scores
0.49/0.55/0.41 — below every zero-false-refusal threshold).
The deterministic rules (`OV-01`, `OV-02`) MUST refuse each of the four known `--lane`
duplicate filings when replayed with the `touches` entries their lane work implies: no
measured threshold refuses them without also refusing distinct rows, so the rules — not a
tuned cutoff — are what catch them. The acceptance proof (A3) replays the four pairs and
reads the `OV-01`/`OV-02` sentences; a replay that warns instead of refusing fails the
requirement.

**OV-04** — Refuse nothing distinct on the current open set.
Strength: `MUST` · Layer: `process` · Source: eval verdict (zero-false-refusal bar on the
220 distinct pairs; every Laya signal's negative maximum exceeds every positive value).
Replaying the 220 distinct corpus pairs as checked writes MUST produce zero refusals: every
one lands or warns, none is refused under `OV-01` or `OV-02`. This is the bar the eval set
for the refuse path — hybrid manages 1/7 clean where Laya manages 0/7 on every signal — and
it is why the refuse rules are equalities (`OV-01`) and intersections (`OV-02`) rather than
cutoffs. A new distinct pair the rules refuse is a rule bug, resolved by narrowing the rule
under its own ID (never by adding a score override), with the pair appended to the corpus
under the corpus-first gate `a-9438f17d`.

**OV-05** — Carry `touches` as a shaped, repo-bound field on task rows.
Strength: `MUST` · Layer: `process` · Source: George gate (touches field shape + repo
identity).
`touches` is a list of strings, each at most 512 characters, each a non-empty repo-relative
path prefix or glob; an entry that is empty, absolute, escapes the repo root (`..`), or
exceeds the length is refused with `invalid touches entry {value}: expected a repo-relative
path prefix or glob of at most 512 characters`, and writes nothing. CLI: repeatable
`--touches PATH` on `task add` (list-valued where it authors a set, scalar nowhere — the
`--allowed-model` precedent at `rust/lib.rs:432`-`rust/lib.rs:435`). The field is stored
with the row, returned on reads that already project the row, and preserved byte-for-byte
across the migration in §5. Which repo the paths resolve against — the board's home repo or
the claiming lane's checkout — is OQ-2; until George closes it, resolution is against the
board's home repo and cross-repo filings MUST name their repo explicitly (see `OV-10`).

**OV-06** — Check the four filing verbs, and only they.
Strength: `MUST` · Layer: `process` · Source: George gate (checked verbs).
The overlap check — deterministic refuse first (`OV-01`, `OV-02`), then the warn pass
(`OV-07`) — runs on exactly four verbs: `task add`, `attention raise`, `claim`, and `claim
--next`. `task update` is NOT checked: an edit to a live row is the lane refining its own
work, and refusing it would strand in-progress rows behind a rule meant for new filings; if
a case for checking updates is made, it arrives as a supersession of this requirement with
its own corpus evidence, never as a reinterpretation. Every other write verb is unchecked
and behaves exactly as today.

### Hybrid triage

**OV-07** — Surface hybrid top-k candidates as a warning that never refuses.
Strength: `MUST` · Layer: `process` · Source: George gate (hybrid top-k feeds
`--overlap-ack` triage; warn, never refuse) + eval (recall@3 4/7, @5 5/7, @10 7/7).
After the deterministic pass, each checked write (`OV-06`) queries the hybrid retriever
(ADR-023, unchanged) with the new filing's title, `--limit 50`, and takes the top 10 open
rows on the same board as warn candidates — no threshold, no cutoff: rank position alone
decides membership, because the eval measured that no warn-level cutoff separates the
classes (warn cutoffs admit 60–80% of distinct pairs — alert fatigue;
`/Users/geoyws/work/wt/laya-eval-t-91e669a8/EVAL.md:178-181`). When the candidate set is
non-empty the write still lands; the product answers success plus the warn payload
(`OV-09`). A candidate set that is empty answers success with no payload. No score, however
high, converts a warning into a refusal.

**OV-08** — Acknowledge considered candidates by id, on the record.
Strength: `MUST` · Layer: `process` · Source: George gate (`--overlap-ack` semantics +
audit event).
`--overlap-ack ID` (repeatable) on each checked write (`OV-06`) names the warn candidates
the filer has considered and files anyway. An ack id MUST be a member of the write's own
warn candidate set (`OV-07`): an ack naming any other id — unknown, closed, unreadable, or
simply not a candidate for this filing — is refused with `unknown overlap candidate {value}
for this filing: expected one of the warned ids`, and writes nothing. Each ack appends one
audit event (ADR-029's chain) recording the new row id, the acknowledged candidate id, the
acting `--as` caller, and the candidate's score at warn time. An ack is consumed by its own
write: it silences nothing future, and a later filing that warns the same candidate requires
its own ack.

**OV-09** — Answer every warning with the candidates and their scores.
Strength: `MUST` · Layer: `process` · Source: George gate (warn payload).
The warn payload is a JSON array (under `--json`; a human line per candidate otherwise),
one entry per candidate in rank order, each carrying exactly `id`, `title`, `score`,
`exact_score`, `lexical_score`, `semantic_score`, and `acknowledged` (whether `--overlap-ack`
named it on this write). Scores are the retriever's as read (ADR-023; `rust/search.rs:743`-
`rust/search.rs:785`), rounded for display but never re-thresholded: the payload MUST NOT
omit, reorder, or relabel a candidate on score grounds. Candidates the caller may not read
under tag scope are absent before ranking (the §2 boundary), never present-and-redacted.

### Reach, sweep, and budget

**OV-10** — Prove cross-board reach with git provenance, checked against the local oracle.
Strength: `MUST` · Layer: `process` · Source: George gate (cross-board reach via claim git
provenance with existence-oracle analysis).
A `claim` naming a task on another board MUST carry `--repo REPO --commit SHA`: the lease
records the pair as the provenance of the reach. The product checks both halves against the
local existence oracle only — the repo is a board the registry knows, the commit is an
object present in that repo's local store — and refuses `unknown overlap reach {value}:
expected a known repo holding the named commit` when either half is unknown, writing
nothing. The check MUST NOT fetch, dial, or resolve anything over the network: an
unreachable remote and a missing object refuse identically, and no availability claim about
the remote is made or implied. Same-board claims carry no provenance and behave exactly as
today.

**OV-11** — Sweep the board for candidate doubles from the CLI only.
Strength: `MUST` · Layer: `process` · Source: George gate (sweep command, CLI only — no
web panel: serve retired under ADR-053).
`kanban overlap sweep [--json] [--limit N]` runs the `OV-07` warn pass for every open row
against every other open row on the board and reports candidate pairs in rank order with
their scores — the same payload shape as `OV-09`, keyed by the newer row of each pair. It
is read-only: it merges, closes, rewrites, and acknowledges nothing, and `task update`
stays unchecked (`OV-06`) whatever the sweep reports. There is no web panel, no served
route, and no browser surface for this or any requirement in the slice (ADR-053); tag scope
applies to the sweep's reads as it does to every read (§2 boundary), and pairs whose newer
row the caller may not read are absent.

**OV-12** — Screen every checked write inside the filing budget.
Strength: `MUST` · Layer: `process` · Source: George gate (per-write budget) + eval
latency (hybrid retrieval 0.08–0.09s server-side at 1-min load 7.82, ~0.5–0.6s end-to-end
via `kb-board`; Laya k=5 judging ≈ 40s per filing).
The deterministic pass plus the hybrid warn pass on one checked write MUST complete within
2 seconds end-to-end on the filing path — headroom over the measured hybrid 0.1–0.6s, and
an order of magnitude below what any judge-shaped step costs (≈ 40s for k=5, which is why
§2 keeps it out). The number is a budget because the measurement behind it is real (not an
invention under ADR-047 §9): the eval's hax timings above are the observation, this
requirement is the commitment. A checked write that cannot finish screening inside the
budget fails closed — refused with `overlap screen timed out after 2 seconds: retry the
filing; nothing was written` — rather than landing unscreened.

## 4. Acceptance examples

### A1 (`OV-01`) — the exact re-filing is refused, the near-miss lands

*Given* open task `t-aaa` titled `Fix the login retry loop` on board `kanban`,
*when* a lane files `kanban task add "  fix   the LOGIN retry loop " --as lane-1 --json`,
*then* the command exits non-zero, writes no row and no event, and answers `duplicate of
t-aaa: this filing has the same title after normalisation`.
*And when* the lane files `kanban task add "Fix the login retry loop v2" --as lane-1
--json`, *then* the row lands (with a warn payload or without one per `OV-07`, never a
refusal).

### A2 (`OV-01`, `OV-02`, `OV-06`) — path collision refuses, empty touches never collide

*Given* open task `t-bbb` with `--touches "rust/store.rs"`,
*when* a lane files `kanban task add "Rebuild the store cache" --as lane-1 --touches
"rust/store.rs" --json`, *then* the command exits non-zero with `touches overlap with
t-bbb: rust/store.rs` and writes nothing — even though the titles differ.
*And when* the lane files the same title and body with no `--touches`, *then* `OV-02` does
not fire (emptiness is not agreement); the write lands or warns on its title alone.
*And when* the lane files the same title and body with `--touches "rust/*.rs" --json`,
*then* the command exits non-zero with `touches overlap with t-bbb: rust/*.rs` and writes
nothing: the glob expands against the lane's checked-out tree at write time (`OV-02`
precedence rule 2 — the filing carries no provenance), matches `rust/store.rs`, and
collides.
*And when* the lane runs `kanban task update t-bbb --title "Fix the login retry loop"
--as lane-1`, *then* no overlap check runs at all (`OV-06`): the edit lands.

### A3 (`OV-03`) — the four `--lane` duplicates are refused by rule

*Given* open task `t-14e6feb7` titled `Accept lane on attention raise for planner queue
routing` carrying `--touches "rust/lib.rs"` — the attention-raise CLI surface every
filing below walks (the `attention raise` grammar at `rust/lib.rs:202`-`rust/lib.rs:204`;
the `touches` entries are what each lane's work implies, pinned here so the replay is
self-contained),
*when* each of the following four checked writes is filed, *then* each is refused as
stated — a warn where a refusal is owed fails this scenario:
* (self re-filing) `kanban task add "Accept lane on attention raise for planner queue
routing" --as lane-1 --touches "rust/lib.rs" --json` is refused with `duplicate of
t-14e6feb7: this filing has the same title after normalisation` (`OV-01`; the title is
byte-identical after normalisation, so the title rule fires first).
* (`t-403f6e88` replay) `kanban task add "DRAFT: Support explicit lane on attention
creation" --as lane-1 --touches "rust/lib.rs" --json` is refused with `touches overlap
with t-14e6feb7: rust/lib.rs` (`OV-02`: exact-path collision; the titles differ after
normalisation, so `OV-01` does not fire).
* (`t-1fce7662` replay) `kanban task add "Add the required explicit lane field to
attention writes" --as lane-1 --touches "rust/*.rs" --json` is refused with `touches
overlap with t-14e6feb7: rust/*.rs` (`OV-02`: the glob expands against the lane's
checked-out tree at write time per precedence rule 2 and matches `rust/lib.rs`).
* (`t-9eafe212` replay) `kanban task add "kb att raise rejects --lane although the kb
skill documents it (SKILL.md:240,333)" --as lane-1 --touches "rust/" --json` is refused
with `touches overlap with t-14e6feb7: rust/` (`OV-02`: the prefix covers everything
beneath it, including `rust/lib.rs`; no tree read).
The four titles are the corpus-resolved titles recorded in the eval inputs
(`laya-inputs-titles.jsonl` in `/Users/geoyws/work/wt/laya-eval-t-91e669a8/`, resolved
live from the board at eval time with zero skips) — not placeholders, and the
implementation run replays them verbatim. The scenario documents the bar the eval set:
hybrid ranks 3/3/6 at 0.49/0.55/0.41 and Laya noul 0.55–0.84 full-body, so no measured
threshold catches them cleanly; the rules do.

### A4 (`OV-04`) — the distinct 220 land or warn, never refused

*Given* the 220 distinct corpus pairs, each replayed as a checked write against the current
open set,
*when* each is filed, *then* zero are refused: each lands, with or without a warn payload.
Any refusal is a rule bug under `OV-04`, fixed by narrowing the rule with a supersession —
never by bolting on a score override.

### A5 (`OV-05`) — malformed touches are refused before anything is written

*Given* any board,
*when* a lane files `kanban task add "Docs pass" --as lane-1 --touches "/abs/path"
--touches "../escape" --json`, *then* the command exits non-zero with `invalid touches
entry /abs/path: expected a repo-relative path prefix or glob of at most 512 characters`
(naming the first bad entry), and writes nothing.

### A6 (`OV-07`, `OV-09`) — the similar filing lands with scored candidates

*Given* open rows whose titles share words with `Reconcile the ledger backlog`,
*when* a lane files exactly that title with no `--overlap-ack`,
*then* the row lands AND the answer carries the warn payload: a rank-ordered array of
`id`/`title`/`score`/`exact_score`/`lexical_score`/`semantic_score`/`acknowledged:false`
entries for the top-10 open rows — including low scores, because no cutoff filters them.

### A7 (`OV-08`) — acknowledgement is per-write, per-candidate, and on the record

*Given* the A6 filing warned `t-ccc` and `t-ddd`,
*when* the lane refiles with `--overlap-ack t-ccc --overlap-ack t-ddd`, *then* the row
lands with both payload entries `acknowledged:true`, and the audit journal carries two
chained events naming the new row, each candidate, the `--as` caller, and each warn-time
score.
*And when* the lane files with `--overlap-ack t-zzz` (not a warned candidate), *then* the
command exits non-zero with `unknown overlap candidate t-zzz for this filing: expected one
of the warned ids` and writes nothing.

### A8 (`OV-08` unauthorised) — an unreadable candidate is absent, not denied in the payload

*Given* a warn candidate set containing a row the caller's tag scope may not read,
*when* the filing lands, *then* the payload omits that row entirely (the §2 boundary); a
payload entry naming it in any form — redacted or otherwise — fails this scenario. The
existing non-enumerating denial still governs direct reads of that row.

### A9 (`OV-10`) — cross-board reach proves provenance or fails closed

*Given* boards `kanban` and `px`, with commit `abc123` present in `px`'s local store,
*when* a lane runs `kanban claim px/t-eee --repo px --commit abc123 --as lane-1 --json`,
*then* the lease records the provenance pair and the claim proceeds by the claim rules as
today.
*And when* the lane names commit `deadbee` (absent locally), *then* the command exits
non-zero with `unknown overlap reach deadbee: expected a known repo holding the named
commit`, writes nothing, and performs no fetch — a packet capture showing any network
dial during the check fails this scenario.

### A10 (`OV-11`, `OV-12`) — sweep reports doubles read-only inside the budget

*Given* a board with open rows including a known duplicate pair,
*when* the operator runs `kanban overlap sweep --json`, *then* the pair appears in the
report keyed by the newer row with its scores, no row is merged/closed/rewritten, no ack
is recorded, and each per-write screen that produced the report stayed inside the 2-second
`OV-12` budget.
*And when* a checked write cannot finish screening in 2 seconds, *then* it is refused with
`overlap screen timed out after 2 seconds: retry the filing; nothing was written` rather
than landing unscreened.

## 5. Contracts and data

- **Interface version or schema:** the CLI grammar pinned in §3 — repeatable `--touches
  PATH` on `task add`; repeatable `--overlap-ack ID` on `task add`, `attention raise`,
  `claim`, `claim --next`; `--repo REPO --commit SHA` on cross-board `claim`; `kanban
  overlap sweep [--json] [--limit N]`; `--as` on every write and `--json` on every verb per
  the `COMMANDS` conventions (`rust/lib.rs:124`-`rust/lib.rs:292`). No served route exists
  (ADR-053) and none is added; the MCP surface projects the same verbs through the
  generated adapters (ADR-010) with no new trust boundary.
- **Data invariants:** `touches` entries are repo-relative, non-empty, at most 512
  characters, stored with the row and returned byte-for-byte; ack events are hash-chained
  (ADR-029) and name new row, candidate, actor, and warn-time score; a lease with
  cross-board reach always carries its provenance pair; refusals write nothing anywhere.
- **Migration:** one forward migration to board schema version 37 carrying `touches`
  storage onto existing rows (empty set — present behaviour for old rows is no entries,
  which under `OV-02` collide with nothing) plus the ack-event and provenance-record
  tables/columns, following the established re-run-safe rebuild pattern
  (`rust/db.rs:2007`-`rust/db.rs:2041`) so a rewound `user_version` cannot collide with
  already-present columns. No registry migration: registry schema stays 14. A
  failed migration step leaves the board at its prior `user_version` with prior data
  intact, because the version bump commits only with the step (the `INCIDENT-14`
  precedent).
- **Compatibility:** boards at version ≤ 36 read and write exactly as today until they
  migrate; older CLI binaries against a version-37 board receive the `database version N is
  newer than supported` refusal through the existing `migrate` path
  (`rust/db.rs:3360`-`rust/db.rs:3412`), not a new sentence.
- **Ownership:** task and attention rows stay owned by their boards; the hybrid index stays
  owned by search (ADR-023); the audit chain stays owned by the journal (ADR-029). This
  slice owns only the two refuse rules, the warn payload shape, the ack semantics, the
  provenance check, and the sweep report.

## 6. Quality and security

- **Reliability:** every refusal in §3 is fail-closed and writes nothing (ADR-008); the
  timeout path (`OV-12`) refuses rather than landing unscreened; migration failure leaves
  the prior version and data intact (§5).
- **Accessibility:** N/A — the slice ships no visual surface; all output is CLI text and
  JSON.
- **Privacy:** warn payloads and sweep reports omit unreadable rows before ranking (§2
  boundary); no redacted-or-named half-state ever ships.
- **Security:** the existence oracle is local-only. The `OV-10` provenance check consults
  the registry and the repo's local object store and MUST NOT emit any network traffic —
  no fetch, no DNS, no reachability probe — so a missing object and an unreachable remote
  are indistinguishable by design, and no prompt, payload, or log line carries remote
  content. `touches` globs expand against the `OV-02` expansion tree only — the repo tree
  at the provenance commit when the filing carries provenance, otherwise the filing
  lane's checked-out tree — and host-style opaque strings are never dialled (the
  `INCIDENT-05` precedent). Tag
  authorization is unchanged and non-enumerating (`rust/authz.rs:34`).
- **Operability:** `overlap sweep` is the operator's doubled-board view; it is read-only by
  construction (`OV-11`), so running it during an incident cannot merge anything. Refusal
  sentences name the blocking row id, so a stranded filer knows exactly what to open.
- **Performance:** observation beside the budget, not instead of it — hybrid retrieval
  measured 0.08–0.09s server-side at 1-min load 7.82 (~31 MB RSS) and ~0.5–0.6s end-to-end
  via `kb-board` over 227 searches; deterministic equality/intersection checks are constant
  work beside one indexed lookup per rule; the eval's Laya k=5 ≈ 40s per filing is the
  cost this slice refuses to pay (full timings in
  `/Users/geoyws/work/wt/laya-eval-t-91e669a8/EVAL.md:119-150`). The commitment is `OV-12`
  (2s per checked write, fail-closed on timeout).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Title normalisation edges: do punctuation stripping, non-ASCII case folding, stop-word removal, or stemming join the `OV-01` mapping? | George | open | implementation — the mapping in `OV-01` is the whole rule until closed; any widening arrives as a supersession with corpus evidence |
| OQ-2 | `touches` repo identity: do paths resolve against the board's home repo or the claiming lane's checkout, and what names a cross-repo filing's repo? | George | open | implementation — until closed, `OV-05` resolves against the board's home repo and cross-repo filings name their repo explicitly |
| OQ-3 | Slice approval pointer: is there an explicit OVERLAP slice row under epic `e-c0852fe7` with George's approval (ADR-047 §7), beyond the 2026-09-28 planner-pane planning (`e-e7aa716a`)? | George | open — recorded here, not filed by this document | implementation |

Note on sources: the board rows behind this document (eval task `t-91e669a8`, gate
`a-9438f17d`, note 449, planning `e-e7aa716a`, epic `e-c0852fe7`) and George's 2026-09-28
planning and gates were taken from the delegating contract, not read directly — this
worktree's constraints allow no KB access. An independent reviewer with board access
confirms the quotations against those rows before any gate.

## 8. Verification

Every test name below is planned, not existing: each is marked `planned`, no name is claimed
to exist at the baseline, and no name was enumerated with `cargo test -- --list` because
there is no overlap test to enumerate — a `grep` for `overlap` over `rust/` and `tests/` at
the baseline finds only prose comments, the import id-overlap refusal, and lock-module
`touch` vocabulary. Each is a compiled-binary exchange at the named layer, landing in the
`## Requirements trace — docs/specs/overlap.md` matrix section on implementation. The matrix
carries `none`/`none` rows with `no e2e coverage` until implementation enumerates the real
names; this table is the draft it copies, and the deliberate difference is stated on both
sides rather than hidden.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `OV-01` | `MUST` | `process` | planned: `overlap_title_refuse_names_duplicate_and_writes_nothing` | asserts the A1 sentence, the whitespace/case mapping, and the near-miss landing; `no e2e coverage` |
| `OV-02` | `MUST` | `process` | planned: `overlap_touches_collision_refuses_with_entry` | asserts the A2 sentence, prefix/glob intersection, and empty-touches non-collision; `no e2e coverage` |
| `OV-03` | `MUST` | `process` | planned: `overlap_lane_duplicates_refused_by_rule` | replays the four A3 pairs; a warn-where-refusal-owed fails; `no e2e coverage` |
| `OV-04` | `MUST` | `process` | planned: `overlap_distinct_corpus_never_refused` | replays the 220 distinct pairs; any refusal fails; `no e2e coverage` |
| `OV-05` | `MUST` | `process` | planned: `overlap_touches_shape_refuses_bad_entries` | asserts the A5 sentence and byte-for-byte round-trip; `no e2e coverage` |
| `OV-06` | `MUST` | `process` | planned: `overlap_checked_verbs_four_and_only` | exercises `task add`, `attention raise`, `claim`, `claim --next`, and proves `task update` unchecked; `no e2e coverage` |
| `OV-07` | `MUST` | `process` | planned: `overlap_warn_lands_with_topk_uncut` | asserts landing plus the uncut top-10 payload incl. low scores; `no e2e coverage` |
| `OV-08` | `MUST` | `process` | planned: `overlap_ack_consumed_per_write_and_chained` | asserts the A7 sentences, per-write consumption, and the chained events; `no e2e coverage` |
| `OV-09` | `MUST` | `process` | planned: `overlap_warn_payload_shape_and_order` | asserts fields, rank order, and no score filtering; `no e2e coverage` |
| `OV-10` | `MUST` | `process` | planned: `overlap_cross_board_provenance_fails_closed_offline` | asserts the A9 sentences and zero network dial; `no e2e coverage` |
| `OV-11` | `MUST` | `process` | planned: `overlap_sweep_reports_read_only` | asserts pair reporting, newer-row keying, and no writes; `no e2e coverage` |
| `OV-12` | `MUST` | `process` | planned: `overlap_screen_budget_fails_closed_on_timeout` | asserts the timeout sentence and nothing-written; `no e2e coverage` |

No `chrome` evidence is planned: the slice ships no pages and no browser surface at all
after the 2026-09-29 retirement, so process-boundary exchanges prove what the slice states;
the matrix rows say `no e2e coverage` plainly per the convention at
`docs/testing/compiled-rust-e2e-matrix.md:224-236`.

## 9. Change log

- `2026-09-29` — slice created at `OV-01` .. `OV-12`. First and only entry: the DROP-Laya
  verdict as the founding source (eval `t-91e669a8` note 560), the note-449
  judge-process-model plan recorded as `VOID`, and OQ-1/OQ-2/OQ-3 opened with owner George.
- `2026-09-29` — independent-review fix (no re-architecture). Finding 1 (`OV-02`
  glob-expansion tree): glob expansion was pinned to the filing's provenance commit
  (`OV-10`), which does not exist for `task add`, `attention raise`, or same-board
  claims — the glob half was unimplementable for provenance-less filings. Fixed with an
  expansion-tree precedence: provenance commit when the filing carries cross-board
  provenance, otherwise the filing lane's checked-out repo tree at write time (working
  tree including uncommitted files the lane can see); exact-path/prefix comparison is
  string-only with no tree read, so its behaviour is byte-identical under either tree.
  A2 gains a provenance-less glob case and the §6 security bullet now names the
  `OV-02` expansion tree. Finding 2 (`OV-03`/A3 replay inputs): A3 named only row ids,
  so it was not replayable from the spec alone. Fixed by pinning the four replay
  filings verbatim — corpus-resolved titles plus per-filing `touches` (self re-filing
  refused under `OV-01`; the three `--lane` duplicates under `OV-02` via exact, glob,
  and prefix collision respectively).
