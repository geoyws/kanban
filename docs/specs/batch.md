# Specification: one ssh round trip answers many kb reads, and reads ride inside transact seeing its writes (slice BA)

## 1. Identity and baseline

- **Slice ID:** `BA`. Requirement IDs are `BA-01` .. `BA-12`, stable across wording
  refinements; numbering is by creation and grouping is by topic.
- **Baseline:** `2026-09-29` at commit `15a1d0f` on branch
  `wt/t-ebb79c13-spec`. Every "today" claim below cites the line that
  has it, as `<path>:<line>`, read at the baseline commit in
  `/Users/geoyws/work/wt/kanban-t-ebb79c13-spec-9f2c/`. Board schema at the baseline is `36`
  (`rust/db.rs:2920`). The MCP read-only `batch` exists and is frozen by ADR-041
  (`rust/mcp.rs:548`-`rust/mcp.rs:624`); the CLI has no batch of any kind — the
  usage table lists `kanban transact (--items JSON_ARRAY | --items-file PATH)` and no
  `kanban batch` (`rust/lib.rs:290`); `transact` already accepts read items through the
  lent-board path (`rust/lib.rs:3615`-`rust/lib.rs:3622`). The specification is
  written before the implementation, as ADR-047 §6 requires (per
  `docs/specs/README.md:15-21`).
- **Status:** `SPEC-READY` on 2026-09-29 (independent reviewer applying the SDD §1 exit criteria:
  no findings. Specification readiness only — it authorises neither implementation, nor rollout,
  nor release).
- **Owner (product scope):** George. He alone resolves scope and the open
  questions in §7.
- **Decider (wording of this document):** BatchSpec, the writer of this slice.
- **Sources:**
  - George, 2026-09-29 (epic `e-cf7e5aaa` body, kanban board): "`we need a way for
    our kb commands to accept multiple commands at one time and multiple replies...
    this way we can save on round trips.`" Recorded in that epic as the ADR-047 §7
    approval of a BATCH slice under the SDD rollout. This document files nothing;
    the approval already exists.
  - Epic `e-cf7e5aaa` (kanban board, status `todo`) — the five gaps this slice
    answers: no CLI read batch; no `--items-file` transfer in `kb-board`;
    reads cannot ride `transact`; skills do not batch; outbox decision 5
    (`e-ea261014`) wants the mail notice once per batch.
  - `docs/adr/ADR-041-transact-is-one-atomic-ordered-write-batch.md` — the read-only
    `batch` contract this slice reuses verbatim (item shape §1, bound §8, envelope §2,
    reads-in-transact §5) and the 26,110.698 ms-to-423.932 ms precedent; amended,
    not superseded, by this slice (amendment 2026-09-29 at that ADR's tail).
  - George, 2026-09-29 (epic `e-ea261014` decision 5, kanban board): "`make messages
    appear next to all the replies of any interaction with /kb because the agents
    will always work with kb as well`" — planner rules (a)–(e): identity from
    `--as`/`KANBAN_ACTOR`; object outputs gain top-level `mail`; at most five plus
    a remaining count, never bodies; unread repeats until acked; heartbeat is the
    reliable carrier. Specified as slice OB (`docs/specs/outbox.md:298`-`:330`,
    `OB-14`, `OB-15`).
  - Task `t-ebb79c13` (kanban board, status `in_progress`, child of `e-cf7e5aaa`) —
    this specification's row; its body is the pin list this document answers.
  - Shipped surface at the baseline: `BATCH_LIMIT` (`rust/mcp.rs:460`); the batch
    item schema (`rust/mcp.rs:503`-`rust/mcp.rs:520`); the batch refusals
    (`rust/mcp.rs:565`-`rust/mcp.rs:604`); the per-entry envelope and `isError:
    false` (`rust/mcp.rs:607`-`rust/mcp.rs:624`); the `transact` row
    (`rust/lib.rs:1240`); the transactable gate (`rust/lib.rs:5419`-`rust/lib.rs:5444`);
    the lent-board read path (`rust/lib.rs:3609`-`rust/lib.rs:3622`); the
    `--items`/`--items-file` pair (`rust/lib.rs:5362`-`rust/lib.rs:5391`); the
    `context` read (`rust/lib.rs:1469`); `kb-board`'s `--body-file` transfer
    (`skills/kb/scripts/kb-board:216`-`skills/kb/scripts/kb-board:289`).
  - Measurements 2026-09-29 by the writer (throwaway probes, §6 table): transact
    accepts a read item; a read after a `claim` sees the claim; a failed batch
    rolls back; same-connection SQLite visibility (`sqlite3 3.54.0`); 4 ssh
    one-shots at ~1.43 s against one collapsed call at ~0.39 s from `@@mbp`
    (`geoywsMBP.local`) to the board home (`hax`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's
  evidence table; the matrix section `## Requirements trace — docs/specs/batch.md`
  is the trace of record when it lands (convention at
  `docs/testing/compiled-rust-e2e-matrix.md:198`-`docs/testing/compiled-rust-e2e-matrix.md:210`).

## 2. Purpose and scope

**Intended outcome.** A lane on `@@mbp` answers its read loop in one round trip and its
claim-to-release write sequence in one more: `kanban batch` carries up to 32 reads on
the CLI with the MCP batch's exact shape and envelope, `kb-board` delivers an items
file across machines, reads ride `transact` seeing its uncommitted writes, and the
outbox mail notice appears once per batch envelope rather than once per item.

**Users / actors.**

- **Lane agent (`@:team/board/lane`)** — sends one `batch` for its reads and one
  `transact` for its write sequence from `@@mbp` through `kb-board`, instead of one
  ssh round trip per operation.
- **Clerk / MCP caller** — builds CLI items with the same `{name, arguments}` shape
  already learned for the MCP `batch` tool; no second naming scheme.
- **Coordinator / superdriver** — sees the unread-mail notice once on the batch
  envelope (slice OB), not once per item.
- **George** — resolves the §7 open questions; owns the release measurement
  (`t-cdcfd429`).

**In scope.** `kanban batch --items JSON_ARRAY | --items-file PATH [--json]`; the
`COMMANDS` row that publishes it; read items inside `transact` and what they observe;
the per-item envelope on both; the three refusal classes; `--items-file` transfer in
`kb-board`; the once-per-batch `mail` field; per-item bands and authorization as they
already are.

**Boundaries.** The MCP `batch` tool itself is untouched (ADR-041 freezes it); this
slice adds its CLI twin. Write semantics stay owned by ADR-041: atomicity, ordering,
`$ref`, `batchId`/`batchIndex` stamps. The mail notice's content rules stay owned by
slice OB (`OB-14`); this slice places it once per envelope. Skill rewrites that teach
batching stay owned by `t-f52e9016`. The release gate and the p50-over-20-runs link
measurement stay owned by `t-cdcfd429`.

**Non-goals.**

- No `$ref` back-references in `batch`: reads are independent by construction, and a
  reference language for reads would be a second `$ref` to keep identical to
  `transact`'s. A read needing another read's answer is two batches, or a
  read-then-decide inside `transact` (`BA-06`).
- No batch-level idempotency key: reads do not mutate, so replay is safe without one;
  `transact`'s gap stays ADR-041 §9's, unchanged.
- No web sentence anywhere in this slice: the serve layer is retired with no
  replacement UI (`docs/adr/ADR-053-the-web-view-is-retired.md`), so every `Layer`
  below is `process`.
- No new capability: like the MCP batch, `kanban batch` collapses round trips and
  does nothing a sequence of separate calls could not.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the planned evidence layer: `unit`, `http`,
`chrome` or `process`.

### CLI read batch

**BA-01 — Carry only reads in the MCP batch's exact item shape.**
Strength: `MUST` · Layer: `process` · Source: epic `e-cf7e5aaa` gap 1; ADR-041 §1.
`kanban batch --items JSON_ARRAY | --items-file PATH` takes an ordered array of
`{"name": NAME, "arguments": {…}}` where `NAME` is an MCP tool name and `arguments`
is that tool's argument object exactly as `tools/call` takes it — the schema the MCP
tool publishes at `rust/mcp.rs:503`-`rust/mcp.rs:520` (`required: ["name"]`,
`additionalProperties: false`). The pair is modelled on `--body`/`--body-file`,
refusing both at once, for the reason `rust/lib.rs:5362`-`rust/lib.rs:5391` gives:
one argv string is capped at 128 KiB on Linux, so a 32-item list travels by file.
Permissions: any caller that may run the underlying read may name it as an item.
Failure behaviour: a malformed item shape is a pre-flight refusal naming the index
(`BA-04`, `BA-05`). Data rules: reads only; nothing is written, so there is nothing
to roll back and no ledger event. Quality constraints: one process, one board open —
the whole list answers without a spawn per item.

**BA-02 — Refuse over the bound naming the bound and the count.**
Strength: `MUST` · Layer: `process` · Source: ADR-041 §8 (`rust/mcp.rs:460`).
At most 32 items: the same `BATCH_LIMIT` constant the MCP batch bounds itself with,
not a second number. A 33-item list is refused whole, naming 32 and 33, and nothing
in it runs — the CLI words of `rust/mcp.rs:565`-`rust/mcp.rs:567`. 32 runs.

**BA-03 — Answer every item independently in one envelope.**
Strength: `MUST` · Layer: `process` · Source: ADR-041 §2 (`rust/mcp.rs:607`-`rust/mcp.rs:624`).
Items run sequentially in the order given and every item is attempted. Each item
reports `{"index": N, "ok": true, "result": …}` or `{"index": N, "ok": false,
"error": …}`; `result` is exactly what that operation returns on its own (the
property `rust/mcp.rs:538` states and `tests/e2e.rs:14010` pins for reads). A
per-item failure is not a batch failure: the envelope answers `isError: false`
with exit zero, because a batch that gave up on the first refusal would hide the
remaining answers behind one. The envelope is `{"results": […]}` in order.

**BA-04 — Refuse any write naming the fix, running nothing.**
Strength: `MUST` · Layer: `process` · Source: epic `e-cf7e5aaa` gap 1; `rust/mcp.rs:591`-`rust/mcp.rs:604`.
An item naming a writing operation — including plain `claim` (its `COMMANDS` row
covers every `claim` invocation and plain `claim` writes a lease, pinned at
`tests/e2e.rs:14149`) and including `transact` (whose row says it writes,
`rust/lib.rs:1240`) — refuses the whole batch before anything runs, naming the
offending index and the operation, ending "nothing in it ran", with the fix: run
writes through `kanban transact`. An item naming no such tool is refused the same
way, naming the index and the unknown name.

**BA-05 — Refuse a batch inside a batch, in either direction.**
Strength: `MUST` · Layer: `process` · Source: ADR-041 §1 (`rust/lib.rs:5419`-`rust/lib.rs:5423`).
`batch` and `transact` are both refused as items of either batcher: a batch may not
carry a batch. The refusal is pre-flight, names the index, and runs nothing. The
read-only MCP `batch` refuses `transact` as an item with no new code (its row says
it writes); the CLI twin refuses `batch` (no row resolves) and `transact` (by name)
the same way `transactable` refuses `transact` today.

### Reads inside transact

**BA-06 — Let read items ride transact and see its uncommitted writes.**
Strength: `MUST` · Layer: `process` · Source: ADR-041 §5; probe (a) 2026-09-29 (§6).
A `transact` item naming a read-only operation runs against the batch's own board
(the lent-board path, `rust/lib.rs:3609`-`rust/lib.rs:3622`) and observes the
earlier items' writes: within one transaction on one connection, a statement sees
the transaction's own uncommitted writes. Measured 2026-09-29: `claim` at index 0
then `task_show` at index 1 answered `ok: true` with `claim.agentID` and
`claimedAt` equal to index 0's write, before commit. A read item is reported like
any other item, and a read item's failure fails the batch and rolls it back — a
batch that continued past a read it could not perform would decide on absent
information. The mixed loop this enables is one `transact`: `claim`, `context`,
`checkpoint` (lease via `$ref` to the claim's `/leaseToken`), `note`, `release`.

**BA-07 — Deliver the items file across machines in kb-board.**
Strength: `MUST` · Layer: `process` · Source: epic `e-cf7e5aaa` gap 2;
`skills/kb/scripts/kb-board:216`-`skills/kb/scripts/kb-board:289`.
`kb-board` transfers `--items-file` the way it transfers `--body-file`: the
caller's file is base64-encoded, decoded into a private temp file on the board
host, the flag rewritten to point at it, and the temp file removed whatever the
exit status, so a refusal leaves nothing behind. Forwarding the flag literally
repeats the defect the `--body-file` comment records (the remote reading a caller
path that does not exist there). Without this, a `transact` from `@@mbp` must
inline its JSON in argv under the 128 KiB ceiling.

### Envelope notice, authorization, invariants

**BA-08 — Carry the unread-mail notice once per batch envelope.**
Strength: `MUST` · Layer: `process` · Source: George 2026-09-29, `e-ea261014`
decision 5; slice OB (`OB-14`).
The `batch` envelope and the `transact` envelope each carry one top-level `mail`
notice — never one per item: ids, sender, kind and a one-line headline per
message, at most five plus a remaining count, never bodies. Identity comes from
`--as` or `KANBAN_ACTOR`; a call with no identity carries no notice. Per-item
`result` objects are byte-identical with or without mail: the notice lives beside
`results`, not inside them, so no consumer breaks. Blocked until `OB-14` lands;
this requirement places the notice, it does not redefine it.

**BA-09 — Authorize every item as if it arrived alone, without an oracle.**
Strength: `MUST` · Layer: `process` · Source: ADR-041 §6; ADR-038.
Each item is authorized exactly as if it had arrived alone: the actor is the
item's own (`--as` in its arguments), there is no batch-level actor, and a batch
cannot elevate. A caller who may not perform an item fails that item (`batch`) or
fails and rolls back (`transact`); denial is indistinguishable for unknown,
denied, and not-yet-created rows — the error shape names the fix for the caller's
own misspecification (bad name, bad index, over-cap) and never confirms or denies
the existence of a row the caller may not read.

**BA-10 — Keep per-item bands, keep history settled, write no new tables.**
Strength: `MUST` · Layer: `process` · Source: ADR-037; ADR-021.
Each item keeps its own `--limit`/`--max-chars` bands unchanged (ADR-037): a
listing that would exceed its cap refuses and names `--limit` rather than passing
a first page off as the whole, inside a batch exactly as alone. `batch` archives
nothing, sweeps nothing, and appends no ledger event (ADR-021): a read batch
leaves the operational indexes exactly as it found them; the claim-sweep a board
open performs is the open path's own, not the batch's, and is committed before any
batch scope exists (ADR-041 §3). No schema migration: no new table, column, or
event kind — `batch` needs no `COMMANDS` write row beyond publishing itself, and
`transact`'s row already exists (`rust/lib.rs:1240`).

**BA-11 — Answer byte-identically to the same call alone.**
Strength: `MUST` · Layer: `process` · Source: `rust/mcp.rs:538`; `tests/e2e.rs:14010`.
A batched read's `result` is byte-identical to the result the same call returns
alone. Measured 2026-09-29 over the real link: `task show t-ebb79c13` alone
against index 0 of a 4-read collapsed call, `json.dumps(sort_keys=True)` equal —
`True`, all four items equal. Anything less and the CLI batch would be a second
way to read the board, free to drift from the first.

**BA-12 — Reuse the batch dispatcher and the command table; no second parser.**
Strength: `MUST` · Layer: `process` · Source: `t-034b6a11` body; ADR-010.
The CLI batch resolves names through `listed_read_only` off `COMMANDS`, coerces
arguments through `arguments_for`, and answers through the standalone `call` —
the same three functions the MCP batch uses (`rust/mcp.rs:471`-`rust/mcp.rs:477`,
`rust/mcp.rs:326`, `rust/mcp.rs:424`). No second item parser, no second name
table, no second refusal vocabulary: the CLI words may differ only in naming the
binary (`kanban batch` for `batch`), never in what is refused or which items run.

## 4. Acceptance examples

### A1 (BA-01, BA-03, BA-11 — the read loop in one round trip)

*Given* a board holding task `t-ebb79c13` and a lane on `@@mbp` that needs it four
times (or twelve reads in the real loop),
*when* the lane runs `kanban batch --items-file reads.json --json` with four
`{"name": "task_show", "arguments": {"id": "t-ebb79c13"}}` items over one ssh
session to the board home,
*then* the envelope carries four `{"index": N, "ok": true, "result": …}` entries
in order, each `result` byte-identical to `kanban task show t-ebb79c13 --json`
alone (measured `True` 2026-09-29), and the wall clock is one round trip (~0.39 s
for four) instead of four (~1.43 s).

### A2 (BA-04 — invalid names and writes refused whole)

*Given* any board,
*when* the lane runs `kanban batch` with (i) an item naming `claim`, (ii) an item
naming `transact`, (iii) an item naming no such tool,
*then* each run is refused whole before anything runs, naming the offending index
and the operation, ending "nothing in it ran"; (i) and (ii) name the fix (run
writes through `kanban transact`); the board and the ledger are byte-identical
afterwards.

### A3 (BA-03, BA-09 — mixed failure stays independent)

*Given* a board where the caller may read task A but not task B,
*when* the lane batches `[task_show A, task_show B, task_show A]`,
*then* index 0 and index 2 answer `ok: true` with their rows, index 1 answers
`ok: false` with the existing non-enumerating denial, the envelope answers
`isError: false` with exit zero, and the denial at index 1 reveals nothing about
whether B exists.

### A4 (BA-06 — claim, read, checkpoint, note, release in one transact)

*Given* task `t-0f1cc103` `todo` and unclaimed on a throwaway board,
*when* the lane runs one `transact` of `claim` (index 0), `context` (index 1),
`checkpoint` (index 2, lease `{"$ref": {"item": 0, "path": "/leaseToken"}}`),
`note` (index 3), `release` (index 4, lease `$ref` to index 0),
*then* every item answers `ok: true`; index 1's `context` carries the claim index
0 just wrote (probe (a2) measured exactly this with `task_show`: `claim.agentID`
`probe-agent`, `claimedAt` equal); and with a wrong lease at index 2 instead, the
envelope answers `failedIndex: 2`, `rolledBack: true`, later items `skipped`, and
the board holds no claim, no note, and no ledger event from the batch (probe
measured `failedIndex: 1`, `rolledBack: true`, notes unchanged at 1 pre-batch row).

### A5 (BA-02, BA-05 — over-cap and nested refusals)

*Given* any board,
*when* the lane runs `kanban batch` with 33 reads, or with an item naming `batch`,
*then* the 33-item list is refused naming 32 and 33 and the nested item is refused
naming its index — both whole, both running nothing, mirroring
`tests/e2e.rs:13917` for the bound.

### A6 (BA-08 — mail once per envelope; planned, blocked on OB-14)

*Given* a recipient with two unread addressed messages for the caller's identity
and a 2-read batch,
*when* the lane runs `kanban batch --as lane --json`,
*then* the envelope carries one top-level `mail` notice naming both messages (at
most five plus a remaining count, never bodies) and neither item's `result`
contains a `mail` key. No new test runs until `OB-14` lands (§7 OQ-1).

### A7 (BA-07, BA-10, BA-12 — transfer, bands, and the parser that is not there)

*Given* a 32-item list too long for one argv string on the caller,
*when* the lane runs it through `kb-board` with `--items-file`,
*then* it lands whole on the board home (transferred, never forwarded); an item
whose listing would exceed its cap refuses naming `--limit` while its siblings
answer; and a name the MCP batch refuses is refused by the CLI batch in the same
words, because there is one parser, not two.

## 5. Contracts and data

- **Interface version or schema:** `kanban batch (--items JSON_ARRAY |
  --items-file PATH) [--json]`; envelope `{"results": [{"index", "ok",
  "result"} | {"index", "ok": false, "error"}]}` plus, once `OB-14` lands, one
  top-level `mail` field (`BA-08`). Item schema is the MCP batch's
  (`rust/mcp.rs:503`-`rust/mcp.rs:520`), published through the generated manifest
  off the new `COMMANDS` row. No interface version bump: additive surface only.
- **Data invariants:** a `batch` writes nothing and appends no event; a `batchId`
  in the ledger still always means a `transact` that landed whole (ADR-041 §7). A
  rolled-back `transact` with read items appends nothing, like any rolled-back
  batch.
- **Migration:** N/A — no migration. No new table, column, event kind, or payload
  key beyond the envelope's top-level `mail` (a view, owned by slice OB).
- **Compatibility:** an older client that sends one call per round trip keeps
  working byte-identically; batching is opt-in per call. Array listings are
  unchanged by the mail notice (`OB-14` rule (b)).
- **Ownership:** the board owns the rows batch reads; the registry owns project
  addressing; slice OB owns the `mail` content rules; ADR-041 owns transact
  semantics. This slice owns only the CLI batch surface, the transfer, and the
  once-per-envelope placement.

## 6. Quality and security

- **Reliability:** independent-failure isolation is the reliability argument: one
  refused read never hides its siblings (`BA-03`, A3), and one refused write never
  exists inside a batch (`BA-04`). No retry is specified: reads are safe to retry
  unmodified, which is stated here so no idempotency key is inferred.
- **Accessibility:** N/A — no browser surface; the serve layer is retired (ADR-053).
- **Privacy:** the mail notice carries headlines, never bodies, at most five plus a
  count (`BA-08`); per-item results carry exactly what the standalone call returns,
  no more.
- **Security:** per-item as-if-alone authorization with non-oracle denials
  (`BA-09`); `kb-board` temp files are private and removed on every exit path
  (`BA-07`); a batch cannot elevate and cannot address a second board (the
  `--db`/`--project` selector belongs to the batch, never to an item — the rule
  `rust/lib.rs:5540`-`rust/lib.rs:5552` states for `transact` applies equally).
- **Operability:** one process per batch keeps the board open once; the bound (32)
  keeps one frame from becoming a job (ADR-041 §8). `kb-board` leaves no temp file
  behind on any exit path.
- **Performance:** an observation, explicitly not a budget. Measured 2026-09-29:

| Arm | n | min | p50-ish (mean of runs) | max | Method |
| --- | --- | --- | --- | --- | --- |
| 4 ssh one-shots `@@mbp` → `hax` | 6 | 1.41 s | **~1.43 s** (~357 ms/trip) | 1.46 s | `/usr/bin/time -p`, fresh ssh per call, `BatchMode`, release kb on `hax` |
| 1 ssh, one collapsed call answering the same 4 reads | 6 | 0.38 s | **~0.39 s** | 0.40 s | same; `transact --items-file` of 4 `task_show` (transact accepts reads, probe (a1)) |
| 4 single CLI one-shots, local debug build | 6 | 0.08 s | **~0.09 s** | 0.11 s | `/usr/bin/time -p`, `geoywsMBP.local` M3 Max load ~12.4, throwaway board |
| 1 collapsed call, same 4 reads, local | 6 | 0.03 s | **~0.038 s** | 0.04 s | same |
| MCP session: 4 singles vs 1 batch(4), local | 6+6 | — | **40.5 ms vs 43.0 ms** | — | throwaway Python stdio session, persistent `kanban mcp` |

Read the table for its structure: over the link the win is round trips (~3.7× at
4 reads, growing with N; the 12-read loop pays ~12 trips today); locally the win
is process opens (~2.4×); inside one MCP session there is no local win because the
MCP batch spawns the binary once per entry (`rust/mcp.rs:409`,
`rust/mcp.rs:591`) — its value is the same round-trip collapse, which is why the
CLI batch (`BA-01`: one process) matters beside it. Precedent: ADR-041's four-arm
loop, 26,110.698 ms p50 to 423.932 ms p50. No budget is set; the release p50 over
20+ runs is owned by `t-cdcfd429` (§7 OQ-2).

Probe (a), thrown away after the numbers were taken: (a1) `transact` of one
`task_show` answered `ok: true` — reads are accepted. (a2) `claim` then
`task_show` in one batch: the read's `claim` equalled index 0's write
(`agentID probe-agent`, `claimedAt` equal) — reads see uncommitted writes.
Rollback: good `note` then bad-lease `checkpoint` answered `failedIndex: 1`,
`rolledBack: true`, index 1's sibling `skipped`, notes unchanged — the write
before the failure left nothing. SQLite semantics cited: one `sqlite3` session
with `BEGIN IMMEDIATE`, one `INSERT`, `SELECT` counted 2 (own write visible),
a second connection counted 1 (pre-batch snapshot), `ROLLBACK` returned the
count to 1 — same-connection visibility with cross-connection isolation, which is
exactly what the lent-board path (`rust/lib.rs:3609`-`rust/lib.rs:3622`) relies on.

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | `BA-08` places one `mail` notice per envelope, but the notice's content rules land with slice OB (`OB-14`). If OB-14's shape moves, does BA-08 follow it without a delta here? Proposed default: yes — BA-08 names the placement only. | George | open | implementation of `BA-08` |
| OQ-2 | Release link measurement: p50 over 20+ runs of the 12-read loop and the claim-to-release sequence, one-call-each versus batched, recorded on `t-cdcfd429`. The §6 table (6 runs, 4 reads) is the acceptance baseline, not the release number. | George | open | release gate (`t-cdcfd429`) |
| OQ-3 | Should `kanban batch` later accept `$ref` between its reads? Proposed default: no — reads are independent (`BA-03`); a read needing another read's answer is two batches or a read inside `transact` (`BA-06`). Reopening needs George. | George | open | implementation (default: no `$ref`) |

Slice approval is not an open question: George ordered BATCH in conversation
2026-09-29, recorded as the ADR-047 §7 approval on epic `e-cf7e5aaa` ("`we need a
way for our kb commands to accept multiple commands at one time and multiple
replies... this way we can save on round trips.`"). This document files nothing.

## 8. Verification

Planned only — no test below runs today. Each row is owed by `t-034b6a11` (draft
until this slice is SPEC-READY) and lands in the matrix section
`## Requirements trace — docs/specs/batch.md`. Where there is no browser evidence
the Note says `no e2e coverage` plainly; every surface here is CLI, so every row
does. Test names are proposed, not enumerated (no implementation exists to list).

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `BA-01` | `MUST` | `process` | planned: `cli_batch_answers_reads_in_order_through_one_board_open` | no e2e coverage |
| `BA-02` | `MUST` | `process` | planned: `cli_batch_over_the_bound_is_refused_naming_the_bound` | mirrors `tests/e2e.rs:13917`; no e2e coverage |
| `BA-03` | `MUST` | `process` | planned: `cli_batch_attempts_every_item_and_reports_each_independently` | mixed ok/fail list; no e2e coverage |
| `BA-04` | `MUST` | `process` | planned: `cli_batch_refuses_a_write_naming_the_fix_and_runs_nothing` | `claim` and `transact` as items; no e2e coverage |
| `BA-05` | `MUST` | `process` | planned: `cli_batch_refuses_a_nested_batch_in_either_direction` | no e2e coverage |
| `BA-06` | `MUST` | `process` | planned: `transact_read_item_observes_the_batch_uncommitted_writes` | probe (a2) promoted; no e2e coverage |
| `BA-07` | `MUST` | `process` | planned: `kb_board_transfers_items_file_and_removes_the_remote_temp` | shell-level, over the real link; no e2e coverage |
| `BA-08` | `MUST` | `process` | planned: `batch_envelope_carries_one_mail_notice_and_items_carry_none` | blocked on `OB-14`; no e2e coverage |
| `BA-09` | `MUST` | `process` | planned: `every_batch_item_is_authorized_as_if_it_arrived_alone` | incl. non-oracle denial; no e2e coverage |
| `BA-10` | `MUST` | `process` | planned: `batch_items_keep_their_own_bands_and_the_batch_writes_nothing` | ADR-037/021; no e2e coverage |
| `BA-11` | `MUST` | `process` | planned: `cli_batch_of_twelve_reads_is_byte_identical_to_twelve_single_calls` | the `t-034b6a11` e2e; no e2e coverage |
| `BA-12` | `MUST` | `process` | planned: `cli_batch_and_mcp_batch_refuse_the_same_list_in_the_same_words` | one parser; no e2e coverage |

## 9. Change log

- `2026-09-29` — created: `BA-01`..`BA-12`. New slice; no supersessions.
