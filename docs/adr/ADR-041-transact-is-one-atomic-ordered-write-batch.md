# ADR-041: `transact` is one atomic ordered write batch; the read-only `batch` is untouched

**Status:** Accepted
**Date:** 2026-09-07
**Deciders:** claude@driver under geoyws's 2026-09-07 ruling (`e-321f6350` note 118;
epic `e-b7401e4c`). geoyws stopped GraphQL indefinitely — "hand rolling our own
graphql is a bad idea" — and named the replacement himself: "be able to batch
requests and batch mutations and that would solve most of the problems." He
ruled on the shape of the problem, not on the field names, the envelope or the
transaction discipline; those are decided here and he may supersede any of them
by a later ADR.
**Supersedes:** nothing. [ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md) (a tool
call runs the binary) and
[ADR-038](ADR-038-linux-principal-broker-policy-and-bootstrap.md)
(authorization) stay in force unchanged; this ADR adds one operation inside
both.

## Context

The read-only half of geoyws's sentence already shipped. The MCP `batch` tool
carries up to 32 read-only calls on one request (`rust/mcp.rs:441`,
`rust/mcp.rs:466`), and the benchmark says what that bought: the twelve-read
agent loop from the MBP to the board home goes from **26,110.698 ms p50** on
one fresh ssh connection per read, to 4,370.572 ms with a ControlMaster, to
2,108.341 ms over a persistent MCP session, to **423.932 ms p50 in two requests**
once eleven of the twelve reads travel in one frame
(`docs/testing/graphql-agent-loop-benchmark-v2-batch-2026-09-07.json`, 30
measured iterations after 5 warmups, `arms[].measured.loop_ms.p50`,
`arms[].requests_per_loop`). The body bytes are identical in all four arms
(217,267 per loop), and the per-read digests are equal across arms
(`equivalence.all_equivalent: true`), so the 61× is round trips and nothing
else. `batch`'s own doc comment says the same thing more plainly: "twelve reads
paying ~190 ms of round trip apiece … A batch is those round trips collapsed
into one and nothing else" (`rust/mcp.rs:511`–`rust/mcp.rs:513`).

The writes got none of it. The `/kb` loop's mutations — `claim`, `heartbeat`,
`checkpoint`, `note`, `release`, `handoff create`, `sitrep post` — are one
round trip each, and there is no batch surface for them at any layer: `batch`
refuses a writing tool by name before anything runs (`rust/mcp.rs:574`), and
`tests/e2e.rs:14149` pins that even `claim` is refused, because its `COMMANDS`
row covers every `claim` invocation and plain `claim` writes a lease. Eleven of
the benchmark's twelve reads are batchable; exactly zero of the loop's writes
are (`tests/e2e.rs:14146`). The CLI has no batch of any kind.

So an agent that has just finished a unit of work still pays a full round trip
for the checkpoint, another for the note, another for the release. On the
measured link that is ~190 ms apiece before the board is even opened, and the
board is opened, swept and closed once per invocation (`rust/store.rs:2070`,
`rust/store.rs:2075`).

There is a second cost, and it is the one that decides this ADR's hardest
question. Those writes are not independent: a `claim` yields the lease token
that the `checkpoint` needs, and a `checkpoint` that lands after a `claim` that
did not is not a smaller success — it is a board that lies. Today each command
is its own transaction and its own process, so a two-write sequence that fails
in the middle leaves the first write committed with nothing to say so. Batching
them without deciding what a failure means would take that hazard and make it
routine.

## Decision

One new operation, `transact`, on both surfaces: the MCP tool `transact` and
the CLI `kanban transact`. It carries an ordered list of ordinary operations,
runs them in one process against one open board inside one transaction, and
either all of them land or none of them do.

**Citation convention.** A bare `<file>:NNN` below is a path under the
repository root. Every claim about current behaviour cites the line that has it,
read at commit `58129a6`.

### 1. Item vocabulary: one shape, and it is `batch`'s shape

An item is `{"name": NAME, "arguments": {…}}`, where `NAME` is an MCP tool name
and `arguments` is that tool's argument object exactly as `tools/call` takes it.
The same shape is the CLI's: `kanban transact --items '<json array>'` or
`--items-file PATH`, the array holding the same objects.

**This deviates from the epic's draft, which spelled the first key `tool`.** The
draft is overruled for one reason, and it is the reason ADR-010 exists: the
read-only `batch` already defines an item, and it names that field `name`
(`rust/mcp.rs:487`), requires it (`rust/mcp.rs:496`), refuses any other key with
`additionalProperties: false` (`rust/mcp.rs:497`), and reads it back with
`fields.get("name")` (`rust/mcp.rs:567`). Two spellings for one concept would
mean an agent that has learned to build a batched read cannot reuse the item it
just built, and a reviewer diffing the two paths has to keep both in mind. If
geoyws prefers `tool`, it is a rename of both surfaces in one cut, not an alias
on the new one.

Names are the MCP tool names because they already map 1:1 onto commands:
`tool_name` flattens a command and its subcommand with `_`
(`rust/mcp.rs:219`), and `COMMANDS` is the single table both the tool list and
the CLI parser are generated from (`rust/lib.rs:688`, `rust/mcp.rs:239`). So
`task_move` is `kanban task move`, and no third naming scheme is introduced.
Argument coercion is the existing one: `arguments_for` turns the object into the
argv the CLI would have been given (`rust/mcp.rs:315`), including the boolean
and repeatable-flag rules (`rust/mcp.rs:369`–`rust/mcp.rs:378`).

Two names are refused as items: `batch` and `transact`. `batch` has no
`COMMANDS` row and so already resolves to nothing (`rust/mcp.rs:451`,
`tests/e2e.rs:13893`); `transact` does have one, so it must be excluded by name.
A batch inside a batch is refused, in either direction, at either layer.

### 2. Ordering, refusal and the envelope

Items run sequentially in the order given. Execution stops at the first
failure; every later item is reported `skipped` and is not attempted.

Each item reports `{"index": N, "ok": true, "result": …}` or
`{"index": N, "ok": false, "error": …}` or
`{"index": N, "ok": false, "skipped": true}`. `result` is exactly what that
operation returns on its own — the property `rust/mcp.rs:518` states for
`batch` and `tests/e2e.rs:14010` pins byte for byte — and `error` is the CLI's
own refusal text, which names the fix (`rust/mcp.rs:391`). `skipped` is a
distinct field rather than an `error` string so an agent can tell "refused"
from "never tried" without parsing prose.

The batch reports `{"ok": …, "failedIndex": …, "rolledBack": …, "batchId": …,
"results": [ … ]}`. On success `ok` is `true`, `failedIndex` is `null` and
`rolledBack` is `false`.

Two refusal classes, deliberately different:

- **Pre-flight refusal** — a malformed item, an unknown or forbidden name, an
  over-length list, an unresolvable back-reference. These reuse `batch`'s form
  exactly: an `isError: true` result whose text names the offending index and
  ends "nothing in it ran" (`rust/mcp.rs:556`, `rust/mcp.rs:569`,
  `rust/mcp.rs:582`). There is no `results` array, because there are no results.
- **Execution failure** — an item ran and refused. The envelope above is
  returned with `ok: false`, and the MCP result is marked `isError: true`.

That second point is where `transact` differs from `batch` on purpose. `batch`
always answers `isError: false`, because "a per-entry failure is not a batch
failure" (`rust/mcp.rs:526`–`rust/mcp.rs:528`, `rust/mcp.rs:603`). For
`transact` a per-item failure **is** a batch failure: the batch rolled back, so
nothing happened, and a result that reads as success would be a lie. The
envelope travels as the error's text so the agent gets the flag and the
per-item detail together.

### 3. Rollback: measured, it composes, and atomic is the default

This was the open question, and it is answered by measurement rather than by
reading. Both halves were probed at commit `58129a6`.

**What the code does today.** Every write path opens its own transaction and
commits it: `claim` at `rust/store.rs:4117`–`rust/store.rs:4119`, committed at
`rust/store.rs:4196`; `heartbeat` at `rust/store.rs:4243`–`rust/store.rs:4245`,
committed at `rust/store.rs:4278`; `release` at `rust/store.rs:4285`, committed
at `rust/store.rs:4310`; `checkpoint` at
`rust/store.rs:4369`–`rust/store.rs:4371`, committed at `rust/store.rs:4411`.
All four use `TransactionBehavior::Immediate`, and the `BEGIN IMMEDIATE` lock is
load-bearing rather than incidental: it is what "keeps a concurrent retag from
slipping between the check and the write" (`rust/store.rs:588`) and what lets a
write path read the tag universe "in the SAME snapshot it is about to sweep"
(`rust/store.rs:610`). `add_note` is the exception and it is a defect: it takes
no transaction at all, inserting the note (`rust/store.rs:4319`) and appending
its ledger event (`rust/store.rs:4329`) in autocommit, so those two are not
atomic even with each other.

**Probe A — an outer transaction cannot wrap those methods.** Two independent
refusals, both observed:

- SQLite refuses the nested `BEGIN`. With one `BEGIN IMMEDIATE` open on the
  board connection, a second returns
  `SqliteFailure(Error { code: Unknown, extended_code: 1 }, Some("cannot start a transaction within a transaction"))`,
  and rusqlite's own opener — the one `db.rs` uses at `rust/db.rs:2250` — returns
  the same message. A `SAVEPOINT` in the same position returns `Ok(())`.
- The borrow checker refuses it earlier than SQLite does. Holding an outer
  `rusqlite::Transaction` borrowed from `store.connection` and then calling
  `store.claim(…)` does not compile:
  `error[E0502]: cannot borrow "store" as mutable because it is also borrowed as immutable`.
  `transaction_with_behavior` takes `&mut self` on the connection, so an outer
  transaction object and a `&mut self` write method cannot coexist by
  construction.

**Probe B — the storage layer composes, and the proving assertion passed.** One
outer `BEGIN IMMEDIATE` taken as raw SQL (so no Rust borrow is held across
items), a `SAVEPOINT`/`RELEASE` per item, item 0 performing exactly the writes
`claim` performs (`rust/store.rs:4170`, `rust/store.rs:4181`,
`rust/store.rs:4185`), item 1 exactly those `checkpoint` performs
(`rust/store.rs:4380`, `rust/store.rs:4397`), then a third item failing the way
a real item fails — `require_active_task` on an id that is not there, refusing
with `task t-absent not found` — and one `ROLLBACK`. Inside the scope the reads
saw both writes (1 claim, 1 checkpoint, 2 ledger events). After the rollback:

```
assert_eq!(count(&store, "SELECT COUNT(*) FROM task_claims"), 0,
           "a rolled-back batch leaves no claim");
assert_eq!(count(&store, "SELECT COUNT(*) FROM checkpoints"), 0,
           "a rolled-back batch leaves no checkpoint");
assert_eq!(count(&store,
           "SELECT COUNT(*) FROM events WHERE kind IN ('task_claimed','checkpoint_added')"), 0,
           "and no ledger event");
assert_eq!(status, "todo", "the task is back where the batch found it");
```

All four passed. The ledger rolls back with everything else because it is
written on the same connection: `append_board_event` reads the chain head
(`rust/audit.rs:235`), derives `seq` as `last_seq + 1` (`rust/audit.rs:248`),
inserts the row (`rust/audit.rs:261`) and embeds the search document
(`rust/audit.rs:272`) — all through the caller's connection, which inside the
batch is the batch's transaction. After the rollback the probe printed
`events remaining after rollback = 1, max seq = 1` and
`audit chain after rollback => AuditReport { journal: "board", healthy: true, entries: 1, last_seq: 1, legacy_entries: 0, … errors: [] }`.
Rolling a batch back frees the sequence numbers and leaves no hole: the hash
chain is intact, not merely unbroken-looking.

**Probe C — today's per-command commit is exactly the half-landed board this
ADR exists to prevent.** The same three items through the real `Store` methods:
`store.claim(…)`, then `store.checkpoint(…)`, then
`store.add_note("t-absent", …)` refusing with `task t-absent not found`. The
board afterwards still holds 1 claim and 1 checkpoint. Nothing rolled back,
because there was no scope to roll back to.

**The exact command.**

```
cargo build --locked --bin kanban
cargo test --locked --lib -- --exact \
  store::transact_probe::probe_a_nested_begin_is_refused \
  store::transact_probe::probe_b_one_outer_transaction_rolls_the_batch_back \
  store::transact_probe::probe_c_per_command_commit_leaves_the_batch_half_landed \
  --nocapture --test-threads 1
```

A **lib** test, not an integration test: `rust/lib.rs:35` declares `mod store;`
privately, so `Store` is unreachable from `tests/`, and the probe lived in a
throwaway `#[cfg(test)] mod transact_probe` appended to `rust/store.rs`. It was
deleted and the file restored byte-identically the moment the numbers were
taken; nothing from it is in the tree. Result:
`3 passed; 0 failed; 0 ignored; 499 filtered out; finished in 0.11s`.

**Cost.** Four writes — `claim`, `heartbeat`, `note`, `checkpoint` — 30
iterations each way, wall clock, on the MBP with no network in the path. The
separate arm spawns the debug `target/debug/kanban` four times per iteration
with `--db <board>` and an isolated `KANBAN_DATA_DIR`; the composed arm calls
the four `Store` methods on one board opened once:

| arm | n | min | p50 | p95 | max | mean |
| --- | --- | --- | --- | --- | --- | --- |
| composed: one process, one open board, four writes | 30 | 5.292 ms | **6.102 ms** | 7.427 ms | 7.530 ms | 6.266 ms |
| separate: four `kanban` invocations | 30 | 694.884 ms | **733.373 ms** | 758.592 ms | 760.382 ms | 731.723 ms |

Command: `cargo test --locked --lib -- --exact
store::transact_probe::probe_d_cost_one_process_versus_four --nocapture`. **120×**
on a debug build with no network in it at all. Read the number for its
structure, not its magnitude: the 727 ms of difference is four process starts,
four registry resolutions, four board opens and four claim sweeps
(`rust/store.rs:2059`, `rust/store.rs:2075`, sweeping and committing at
`rust/store.rs:3179`–`rust/store.rs:3183`), none of which the composed path pays
more than once. A release build shrinks the constant; it does not change which
side of the comparison the work is on. Over ssh, each of the four also pays the
~190 ms round trip the read benchmark already measured
(`rust/mcp.rs:512`), which `transact` collapses to one.

**Therefore: atomic by default.** A `transact` whose item `k` fails rolls back
items `0..k` and answers `rolledBack: true`. `rolledBack: false` appears in
exactly one place — a pre-flight refusal, where nothing was attempted and there
is nothing to undo — and it is stated rather than omitted, so an agent never
has to infer it.

**What Phase 2 must build for this to be true**, since Probe A says the current
signatures cannot express it:

1. One write-scope guard on `Store`, replacing every
   `self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?`
   site with `self.begin_write()?`. Outside a batch the guard issues
   `BEGIN IMMEDIATE`; inside one it issues `SAVEPOINT`. Both are raw SQL on
   `&Connection`, which is what sidesteps the `E0502` in Probe A — the outer
   scope is a flag and a SQL statement, never a live Rust borrow held across
   items. The guard derefs to `Connection` so the ~40 existing `&transaction`
   call sites are unchanged, `commit()` becomes `COMMIT` or `RELEASE`, and
   `Drop` without commit rolls back, matching the `DropBehavior::Rollback` the
   read path already relies on (`rust/db.rs:2239`).
2. The batch takes the `IMMEDIATE` lock once, at the start. This is strictly
   stronger than today, not weaker: every item's check-then-write runs under a
   write lock that was already held when the batch began, which is the property
   `rust/store.rs:588` and `rust/store.rs:610` are protecting. A savepoint-only
   design that dropped `BEGIN IMMEDIATE` would weaken it and is refused.
3. `add_note` acquires the guard like everything else, closing the autocommit
   gap at `rust/store.rs:4316`–`rust/store.rs:4329`.
4. One `Store` per `transact`, opened once. The MCP path must **not** reuse
   `batch`'s per-entry `Command::new(path)` spawn (`rust/mcp.rs:409`,
   `rust/mcp.rs:591`): a process per item is a connection per item, and two
   connections cannot share a transaction. `transact` runs the binary **once**
   with the whole item list, which keeps ADR-011's invariant — "a tool call runs
   the binary" (`rust/mcp.rs:5`) — literally true.

**The one thing a rollback does not undo, stated because it is invisible
otherwise.** Opening the board sweeps expired claims and commits that sweep
before any batch scope exists (`rust/store.rs:2070`, `rust/store.rs:2075` →
`rust/store.rs:3179`–`rust/store.rs:3183`). A `transact` that rolls back may
still have retired somebody else's lapsed lease and appended those events. That
is correct — the sweep is not part of the caller's batch and reverting it would
resurrect dead leases — but a `transact` is not a no-op on the ledger in every
possible sense, and Phase 2 must not claim it is.

### 4. Back-references

Any argument value may be `{"$ref": {"item": N, "path": "<JSON Pointer>"}}`,
which is replaced by the value at that pointer inside item `N`'s result.

`N` must be strictly less than the referring item's index. Forward and
self-references are refused pre-flight.

**Paths are RFC 6901 JSON Pointers, not dotted paths.** Three reasons, in order
of weight: `serde_json::Value::pointer` already implements the resolution, so
this introduces no parser of our own; a pointer has defined escaping (`~0`,
`~1`) and defined array indexing, where a dotted path has neither and would
have to invent both; and board payloads carry keys a dotted path cannot express
unambiguously. So the lease token a `claim` returns
(`rust/model.rs:491`, serialized `leaseToken`) is `"/leaseToken"`, and the
`taskID` of the first row of a listing is `"/0/taskID"`.

Every reference in the whole list is resolved for **shape** before any item
runs: that `item` is an integer in range and less than the referring index, that
`path` is a syntactically valid pointer, that `$ref` is the object's only key.
An unresolvable reference refuses the whole batch, names the offending index,
and no item runs — the same fail-closed rule `batch` applies to a forbidden name
(`rust/mcp.rs:522`, "A batch that names one operation it may not perform
performs none of them"). A reference whose pointer finds nothing in a result
that did arrive is an execution failure at the referring index, and rolls the
batch back like any other.

### 5. Reads inside a write batch

Allowed, and they observe the earlier items' writes. Probe B measured exactly
this: inside the open scope, after items 0 and 1, the counts were 1 claim, 1
checkpoint and 2 events. This is what makes a read-then-decide item useful
inside a batch, and it costs nothing to allow: within one transaction on one
connection, a statement sees the transaction's own uncommitted writes.

A read item is reported like any other. A read item's failure fails the batch,
because a batch that continued past a read it could not perform would be
deciding on absent information.

### 6. Authorization

Each item is authorized exactly as if it had arrived alone. The actor is the
item's own (`--as` in its arguments, `author` on a note, and so on); there is no
batch-level actor and a batch cannot elevate. The store's guard is consulted
per write inside the item's own scope, unchanged
(`rust/store.rs:4121`, `rust/store.rs:4247`, `rust/store.rs:4287`,
`rust/store.rs:4316`, `rust/store.rs:4373`), and ADR-038's trusted-edge rules
are untouched.

`transact`'s `readOnlyHint` is `false`, so a harness that withholds mutation
withholds it (`rust/mcp.rs:306` is where that annotation is generated;
`rust/mcp.rs:503` is `batch`'s `true`, which does not change).

The read-only `batch` is not modified in any way. It keeps `readOnlyHint: true`
and it refuses `transact` as an item with no new code: `listed_read_only` reads
the `COMMANDS` row (`rust/mcp.rs:452`–`rust/mcp.rs:457`), that row says
`transact` writes, and the existing arm refuses it whole
(`rust/mcp.rs:574`–`rust/mcp.rs:579`). `tests/e2e.rs:13840` already pins that
behaviour and must keep passing untouched.

### 7. Audit

One ledger event per landed item, in the existing shape, appended by the
existing path (`rust/audit.rs:223`). No new event kind, no new table, no new
column: `batchId` and `batchIndex` are two keys added to the event's `payload`
JSON, beside the `_semanticV1` snapshot the ledger already requires
(`rust/store.rs:1577`).

`batchId` is minted once, at the top of `transact`, before the outer scope
opens: `Uuid::new_v4().to_string()` — lowercase hyphenated v4, the same mint and
the same format as a lease token (`rust/store.rs:4169`). `batchIndex` is the
item's index in the list as given, so a reader can reconstruct the order from
the ledger alone.

A rolled-back batch appends nothing, so a `batchId` in the ledger always means
a batch that landed whole. Probe B measured that: zero events after the
rollback, chain healthy, `max seq` back where it started.

### 8. Limits

`BATCH_LIMIT` is reused, not duplicated: at most 32 items, the same constant
`batch` bounds itself with (`rust/mcp.rs:441`), for the same reason its doc
comment gives — "an unbounded array would put an unbounded amount of work
behind a single request" (`rust/mcp.rs:436`). Over the bound is a pre-flight
refusal naming the bound and the count, exactly as `rust/mcp.rs:546` already
does.

Each item keeps its own argument bands unchanged — `--limit` and `--max-chars`
are validated per operation (`rust/lib.rs:3210`, `rust/lib.rs:6145`) under
ADR-037 — and `transact` adds no ceiling of its own. There is no byte ceiling in
the MCP layer today and this ADR does not invent one: the read batch's 217 KB
loop is a reading concern, and write receipts are small.

One real ceiling does apply and it shapes the surface: the item list travels as
a single argv string if passed with `--items`, and Linux caps one argv string at
128 KiB (`MAX_ARG_STRLEN`, a kernel constant, not measured here). So the CLI
takes `--items TEXT | --items-file PATH`, the pair modelled exactly on
`--body`/`--body-file` including refusing both at once
(`rust/lib.rs:1809`–`rust/lib.rs:1821`), and the MCP tool always uses
`--items-file` with a temporary file it removes, so a 32-item batch carrying
checkpoint bodies cannot fail as `E2BIG`.

### 9. Idempotency: none, and here is what that means

`transact` has no idempotency key, no request id and no dedupe in V1. Each item
carries its own semantics, and the batch adds nothing.

Spelled out, because the failure mode is easy to get wrong: **a replayed batch
is not a no-op.** The interesting case is the loop batch that begins with a
claim. A replayed `claim` is refused — an active claim makes the second attempt
fail by name, telling the caller who holds it and how to take it
(`rust/store.rs:4149`, `rust/store.rs:4155`) — so a replayed loop batch fails at
index 0, every later item is `skipped`, and because the batch is atomic it lands
nothing. That is the good case and it is good by accident of `claim`'s own
semantics, not by anything `transact` does.

The bad case is a batch whose first item is additive. Two `note` items replayed
are two notes; two `sitrep post` items replayed are two sitreps. An agent that
retries a `transact` after a transport failure must therefore either (a) begin
the batch with an item that refuses on replay, or (b) read the board first and
decide. A batch-level idempotency key would remove that reasoning entirely and
is the obvious V2; it is deliberately not smuggled into V1, where it would need
a durable key store, an expiry policy and a decision about what a replay
*returns*.

### 10. The name is `transact`

MCP tool `transact`; CLI `kanban transact`. It says what the operation
guarantees rather than how it is packed, which is the difference that matters
here: `batch` promises order and one round trip, `transact` promises all-or-
nothing. Naming both "batch" would make the atomicity invisible at the call
site, and naming this one `batch_write` would suggest the read batch's
per-entry-failure semantics, which it explicitly does not have (§2).

### 11. Schema drift cannot hide a missing or renamed surface

`transact` gets a `COMMANDS` row — `("transact", None, &["items", "items-file"],
&[], false)` — which is what makes every generated surface follow:

- `kanban schema --json` lists it, with `readOnly: false` and
  `longRunning: false`, because the manifest is generated from that same table
  (`rust/lib.rs:2358`–`rust/lib.rs:2381`, `rust/lib.rs:2362`,
  `rust/lib.rs:2371`).
- `tests/e2e.rs:10841`,
  `the_schema_describes_the_real_surface_and_read_only_really_is`, fails if the
  parser accepts an operation the manifest does not publish, or the reverse —
  "every operation the parser accepts appears, and nothing else does". A row
  added without a manifest entry, or a rename landed on one surface only, fails
  there.
- `tests/e2e.rs:2015`–`tests/e2e.rs:2029` is the pattern that pins
  `readOnly`/`longRunning` per operation off `schema --json`; the Phase 2 case
  named below extends it to `transact`.
- The MCP tool list needs one hand-written entry and one filter, in the shape
  `batch` already established. `tools()` builds from `COMMANDS`
  (`rust/mcp.rs:239`) and appends `batch_tool()` (`rust/mcp.rs:310`); `transact`
  must be filtered out of the generated map beside the `LONG_RUNNING` filter
  (`rust/mcp.rs:241`) and appended as `transact_tool()`, because its input is a
  list of calls rather than a set of flags — the same reason `batch_tool` is
  hand-written (`rust/mcp.rs:462`–`rust/mcp.rs:465`). Its `annotations` carry
  `readOnlyHint: false`.
- `listed_read_only("transact")` answers `Some(false)` from the `COMMANDS` row,
  which is what makes §6's "the read-only batch refuses it" true with no new
  code.
- Documentation drift is caught too: the skill is read back into the test binary
  with `include_str!("../skills/kb/SKILL.md")` (`rust/lib.rs:7236`), so an alias
  documented for a command that does not exist fails, and `rust/lib.rs:7312`
  reads `lib.rs` itself back to check every declared flag is really parsed.

## Consequences

**Phase 2 (`t-77e00737`) owns the store and the CLI.** The write-scope guard
(§3), `add_note`'s missing transaction (§3.3), `kanban transact` with
`--items`/`--items-file`, the `COMMANDS` row, the envelope, the `$ref`
resolution pass, and `batchId`/`batchIndex` on the ledger payload. The guard is
the largest piece and it is mechanical: one helper, ~40 call sites, no change to
any write path's logic.

**Phase 3 (`t-ffab5763`) owns the MCP surface.** `transact_tool()`, the
generated-map filter, dispatch that runs the binary **once** with
`--items-file`, the temp-file lifecycle, and `isError: true` on a failed batch.
(This ADR assigns that split; if the board rows divide the work differently, the
acceptance cases below still bind whichever row owns each surface.)

**What the `/kb` loop becomes.** The end-of-unit sequence — checkpoint, note,
release — becomes one call, and the start-of-unit sequence — claim, heartbeat,
first note — becomes another, with the lease token flowing from the claim to the
later items by `$ref` instead of through the agent. On the measured link that is
two round trips where there were six, and on the local numbers 6.1 ms of board
work where there were 733 ms of process work. The kb skill's write examples
become batches; nothing about the individual commands changes, so the
single-command examples stay correct.

**What is deliberately still slow.** `transact` collapses round trips. It does
not make a write cheaper, does not add a new capability, and cannot do anything
a sequence of separate calls could not — with the single exception that it can
now fail *cleanly*, which is a capability the separate calls never had.

**The idempotency gap is real and named.** V1 has no batch-level replay
protection (§9). The mitigation is a property of the items an agent chooses to
put first, which is a sharp edge. A V2 idempotency key is the follow-up; until
it exists, an agent that cannot tell whether a `transact` was received must read
the board rather than retry blindly.

**The read-only `batch` is frozen by this ADR, not extended.** Any future
temptation to let `batch` carry a write must come back here first: its whole
safety argument is that it refuses one (`rust/mcp.rs:574`), and
`tests/e2e.rs:13840` is the assertion that keeps it honest.

## Acceptance: the compiled-process cases Phase 2 and Phase 3 must pin

Named so the implementers cannot ship a narrower proof. All of these are
compiled-process e2e cases in `tests/e2e.rs`, driven through the real binary
and — for the MCP ones — a real stdio `Session`, using the existing helpers
`batch_results` (`tests/e2e.rs:13758`) and `batch_refusal`
(`tests/e2e.rs:13775`) as the model for `transact_results` / `transact_refusal`.

Phase 2, CLI and store:

1. `transact_runs_its_items_in_order_and_each_result_matches_the_same_command_alone`
2. `a_transact_that_fails_midway_rolls_back_every_item_before_it` — the board
   after the failure has no claim, no checkpoint, no note, and no ledger event
   from the batch; this is Probe B's assertion promoted to the compiled surface.
3. `a_failed_transact_leaves_the_audit_chain_healthy_and_its_sequence_unbroken`
   — `kanban audit verify --json` is healthy and `max(seq)` is what it was
   before the batch.
4. `a_transact_reports_failed_index_rolled_back_and_skips_every_later_item`
5. `a_transact_resolves_a_back_reference_to_an_earlier_items_result` — the lease
   token from a `claim` at index 0 reaching a `checkpoint` at index 2 by
   `{"$ref": {"item": 0, "path": "/leaseToken"}}`.
6. `a_transact_with_an_unresolvable_reference_is_refused_whole_and_runs_nothing`
   — forward reference, self-reference, out-of-range index and malformed
   pointer, each naming its index, each leaving the board and the ledger
   byte-identical.
7. `a_read_inside_a_transact_observes_the_earlier_writes`
8. `a_transact_naming_batch_or_transact_is_refused_whole`
9. `a_transact_over_the_bound_is_refused_naming_the_bound` — 33 refused naming
   32 and 33; 32 runs, mirroring `tests/e2e.rs:13917`.
10. `every_item_of_a_transact_is_authorized_as_if_it_arrived_alone` — an item
    whose actor may not perform it fails that item and rolls the batch back;
    the batch cannot elevate.
11. `a_landed_transact_stamps_batch_id_and_batch_index_on_every_event_and_a_rolled_back_one_stamps_none`
12. `the_schema_lists_transact_as_a_writing_operation` — extends
    `tests/e2e.rs:2015`'s pattern; `readOnly: false`, `longRunning: false`.
13. `a_transact_whose_items_exceed_one_argv_string_still_runs` — the `--items-file`
    path, with a list large enough that `--items` would be `E2BIG`.
14. `add_note_is_atomic_with_its_ledger_event` — the regression that closes
    `rust/store.rs:4316`–`rust/store.rs:4329`.

Phase 3, MCP:

15. `the_tool_list_offers_transact_once_with_read_only_hint_false` — exactly one
    entry named `transact`, no generated duplicate.
16. `a_transacted_write_is_identical_to_the_same_write_on_its_own` — the
    `tests/e2e.rs:14010` property, for writes.
17. `a_failed_transact_answers_is_error_true_and_carries_the_envelope`
18. `a_transact_runs_the_binary_once_for_the_whole_list` — the property §3.4
    rests on; a per-item spawn cannot share a transaction, so this is the test
    that keeps atomicity from silently regressing.
19. `the_read_only_batch_still_refuses_transact_and_every_other_writing_tool` —
    `tests/e2e.rs:13840` unchanged, plus `transact` by name.

## References

- `rust/mcp.rs:433`–`rust/mcp.rs:605` — the read-only `batch`: the bound, the
  hand-written tool, the pre-flight validation, the dispatch, the envelope
- `rust/mcp.rs:5`, `rust/mcp.rs:11` — ADR-011's two invariants, both preserved
- `rust/mcp.rs:219`, `rust/mcp.rs:239`, `rust/mcp.rs:315` — tool names,
  generation from `COMMANDS`, argument coercion
- `rust/lib.rs:688`, `rust/lib.rs:2358`–`rust/lib.rs:2381` — the command table
  and the manifest generated from it
- `rust/store.rs:588`, `rust/store.rs:610` — why the write lock is `IMMEDIATE`
- `rust/store.rs:2052`, `rust/store.rs:2070` — the two open paths; both sweep
  and commit at `rust/store.rs:3179`–`rust/store.rs:3183`
- `rust/store.rs:4112`, `rust/store.rs:4233`, `rust/store.rs:4282`,
  `rust/store.rs:4314`, `rust/store.rs:4367` — `claim`, `heartbeat`, `release`,
  `add_note`, `checkpoint`
- `rust/db.rs:2240`–`rust/db.rs:2254` — `read_snapshot`, and why a transaction
  behaviour is named rather than inherited
- `rust/audit.rs:223`–`rust/audit.rs:273` — the hash chain and the in-transaction
  search embed that let a batch roll back cleanly
- `tests/e2e.rs:13758`, `tests/e2e.rs:13775` — the batch test helpers to model
- `tests/e2e.rs:13840`, `tests/e2e.rs:13917`, `tests/e2e.rs:13953`,
  `tests/e2e.rs:14010` — the four read-batch cases; the first must keep passing
  unchanged
- `tests/e2e.rs:10841` — the schema/surface drift guard
- `docs/testing/graphql-agent-loop-benchmark-v2-batch-2026-09-07.json` — the
  four-arm loop measurement quoted in the Context
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — one surface,
  one generated schema; the reason §1 reuses `batch`'s item shape
- [ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md) — a tool call runs the binary
- [ADR-026](ADR-026-claim-candidates-are-a-read-only-scheduler-view.md) — why
  `claim --candidates` reads and `claim` writes
- [ADR-029](ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md) —
  the chain a rolled-back batch must not hole
- [ADR-037](ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md) —
  the per-operation bands §8 leaves alone
- [ADR-038](ADR-038-linux-principal-broker-policy-and-bootstrap.md) — the
  authorization §6 does not change
- Kanban board: epic `e-b7401e4c`; task `t-51ad33ba` (this ADR); Phase 2
  `t-77e00737`; Phase 3 `t-ffab5763`; geoyws's ruling `e-321f6350` note 118
  (2026-09-07)
- Measured at commit `58129a6` on `kanban-geoyws-driver`
