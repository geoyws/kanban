# ADR-031: Ledger-first pub/sub uses the append-only event ledger

**Status:** Accepted
**Date:** 2026-08-30
**Deciders:** George

## Context

Kanban already has an append-only event trail for boards and a separate
append-only trail for registry rules. In the board schema, `events` is
`seq INTEGER PRIMARY KEY AUTOINCREMENT`, `Store::events` reads it newest-first,
and `append_board_event()` writes `seq = last_seq + 1` together with the
previous hash and current hash. The registry trail follows the same pattern in
`rule_events`. Audit verification walks each trail in sequence order and checks
the hash chain.

That is enough to define a canonical pub/sub substrate without introducing a
broker, a filesystem watcher, or another mutable projection. The current web
surface already has a lightweight live-notice path: `kanban serve` fingerprints
the registered SQLite database plus `-wal` and `-journal`, sends `ready`,
`refresh`, and heartbeat frames on `/live`, and then fetches the canonical
server-rendered page again. That path remains compatibility invalidation for
the current UI, not the canonical stream. The canonical long-running
subscription is `kb watch`, which reads the append-only ledgers directly while
`kb events` stays the newest-first snapshot reader.

The CLI already exposes the underlying history through `kb events` for a task,
a rule, or the registry, and the binary already treats long-running surfaces
such as `mcp` and `serve` as processes rather than request/response tools. The
missing piece is one canonical, process-separated subscription contract that
downstream adapters and UI consumers can share instead of inventing their own
polling logic.

## Decision

### 1. The ledger is the bus

The canonical pub/sub source is the existing append-only event ledger for the
addressed scope. No separate queue, broker, socket-fed state store, or
filesystem change detector becomes authoritative.

For board scope, the bus is the board database's `events` table. For registry
scope, the bus is the registry database's `rule_events` table. A subscriber is
bound to exactly one scope at a time; there is no cross-board fan-in.

The watch cursor is opaque. It binds the exact source, selector, predicate set,
archive state, and last consumed ledger `seq` together. Consumers persist the
cursor from every event or advancing-heartbeat envelope and resume from the
next ledger row. Literal `0` is the only bootstrap cursor. Wall clock time,
file mtime, WAL fingerprints, and payload hashes are not cursors.

Reusing a cursor against a different scope, selector, kind, archive state, or
future sequence must fail closed rather than silently replaying the wrong
trail.

### 2. `kb watch` is a process, not an RPC tool

`kb watch` is the canonical long-running subscription command. It is read-only
and process-separated, and the generated command schema marks it
`longRunning`/`readOnly` so MCP tool generation excludes it the same way it
excludes the other long-running surfaces. Downstream adapters that want a
subscription must spawn it or bridge it, not call it as an in-process
function.

The contract is:

```text
kb watch [--project NAME] [--task ID | --rule ID | --registry]
         [--kind KIND ...]
         [--relation parent:ID|ancestor:ID|depends-on:ID ...]
         [--prior-status STATUS ...]
         [--current-status STATUS ...]
         [--tag TAG ...]
         [--cursor CURSOR]
         [--follow]
         [--all]
         [--limit N]
         [--db PATH]
         [--json]
```

Semantics:

- Exactly one scope is active at a time. `--project` selects the addressed
  board. `--task` is the subject selector. `--rule` narrows the registry
  `rule_events` trail to the selected rule ID. `--registry` selects the
  registry trail directly. There is no cross-board fan-in.
- `--kind`, `--relation`, `--prior-status`, `--current-status`, and `--tag`
  are repeatable predicates. `--relation` accepts typed forms `parent:ID`,
  `ancestor:ID`, and `depends-on:ID`.
- Values within one predicate family are ORed. Families are ANDed together.
- The command normalizes the full predicate set before binding it to the
  cursor. Reusing a cursor with any different normalized predicate set fails
  closed.
- Unknown task IDs, relation targets, kinds, statuses, or tags fail closed.
- Removed subjects and historical relation targets remain replayable because
  replay uses stored rows, not live lookups.
- Registry scope supports `--kind` only and rejects board semantic predicates.
- `--cursor` is the opaque resume point. Literal `0` is the only bootstrap
  cursor. The watcher replays rows with sequence numbers greater than the last
  delivered one and never writes the cursor itself.
- The cursor is bound to the exact source, selector, normalized predicate set,
  archive state, and last consumed ledger `seq`. Reusing a persisted cursor with a
  different source, selector, normalized predicate set, archive state, or
  future `seq` fails closed.
- `--follow` keeps the process open after replaying the backlog. Each poll
  reopens a read-only database transaction, reads the committed rows
  synchronously with no intermediate queue, and then closes before the next
  poll. The runtime requires `--limit` to be at least `1` whenever `--follow`
  is set, so `--follow --limit 0` fails.
- `--limit` bounds replay work and must be within `0..1000` in every mode;
  follow mode additionally rejects `0`. Sparse filtering happens before
  `--limit`, so the limit slices the filtered result set rather than the raw
  rows.
- `--db PATH` opens that exact database file rather than re-resolving a board.
- `--json` is the machine contract. The stream stays NDJSON on stdout; errors
  and diagnostics belong on stderr.
- Idle heartbeats do not advance the durable cursor. When predicates skip a
  committed tail with no matching event, an `advanced` heartbeat moves the
  opaque cursor to the last scanned row so follow mode does not rescan the same
  unmatched rows forever.
- Secrets are redacted recursively before payloads are emitted.

Each delivery is an NDJSON envelope containing:

- `version`, the stable pubsub protocol version, starting at integer `1`;
- `scope`, naming the addressed board or registry trail;
- `cursor`, the last consumed monotonic ledger sequence number;
- `type`, either `event` or `heartbeat`;
- and `payload`, the underlying event JSON shape the current CLI already uses.

Envelope readers must reject incompatible protocol versions and fail closed.
The wire version does not track the changing binary release string.

The event payload is additive protocol-v1. The stored board event shape is
stable, and the emitted payload carries:

- `board`, a stable `{id, name?}` object for board scope and `null` for registry
  scope;
- `eventID` and `eventHash`;
- `seq`, `timestamp`, `actor`, and `kind`;
- `subject`;
- typed `parent`, `ancestor`, and `depends-on` relations;
- `priorStatus` and `currentStatus`;
- sorted registered tags;
- bounded recursively redacted metadata; and
- explicit `null` semantic fields on legacy events.

Stored board event payloads also carry private `_semanticV1`. It is
hash-covered in the board ledger, but it is never emitted on the watch stream.

### 3. Replay, ordering, and liveness are ledger properties

Replay is defined by `seq > cursor` and `ORDER BY seq ASC`. A subscriber that
restarts from a saved cursor must see the same event order every time because
the trail is append-only and sequence numbers are strictly increasing.

Liveness is separate from ordering. If no new rows arrive, `kb watch` emits a
heartbeat frame often enough for callers to distinguish "idle" from "dead".
The watcher does not infer liveness from WAL changes or socket writes.

`Store::events` and `Registry::rule_events` stay bounded newest-first snapshot
readers for inspection. `kb watch` must use new ascending cursor query
primitives for board and registry trails instead of wrapping or reversing those
existing readers. That keeps the inspection commands stable while giving the
watch path a cursor-native API that can be tested independently.

`--follow` is a poll-and-reopen loop over read-only transactions on the
addressed database. One transaction reads a stable snapshot, delivers every row
with `seq > cursor` in ascending order, and then closes before the next poll.
A row committed after one transaction begins becomes visible only after the
next transaction begins. That keeps write transactions uncoupled from
subscribers: no callbacks, no lease, no shared mutable queue, and no writer has
to wait for a reader.

The current web `/live` channel may keep using its filesystem-fingerprint
compatibility notice for now, but its replacement must be cursor-driven and
must use the same ledger semantics as `kb watch`. The compatibility behavior
and the replacement are both rollout items, not an implementation detail to be
implied later. The socket stays a notification channel and never becomes a
second source of truth. Any cursor-driven replacement must preserve the current
draft-reply suppression and the other interaction guards that prevent typing
from being erased or replaced unexpectedly.

The phase-1 compatibility wording above is historical. The canonical stream is
protocol-v1, and `/live` stays compatibility invalidation only.

### 4. Slow consumers get bounded buffering, not hidden data loss

The watcher keeps only bounded in-memory state. It does not build an infinite
backlog for a client that is not reading.

If a consumer falls behind, the correct result is an explicit slow-consumer
failure or a closed stream that the caller can resume from the last durable
cursor. The process may buffer only a fixed backlog of undelivered envelopes;
once that buffer is full, it must stop extending the backlog and fail closed
rather than letting memory grow without bound. That preserves the contract:

- the authoritative ledger remains intact;
- replay after reconnect is deterministic;
- writers are never forced to wait on a subscriber;
- and the consumer owns its own catch-up policy.

### 5. Tenancy and auth stay where they already are

Pub/sub does not create a new trust boundary. A watcher sees only the board or
registry scope it was already allowed to open.

That means:

- one board database or one registry database per watcher;
- no implicit cross-board fan-in;
- no automatic fleet-wide stream;
- no lease tokens or other private capability material in the stream;
- and no bypass of the current filesystem and same-origin boundaries the
  existing CLI and web surfaces already enforce.

The stream is a projection of authorized read access, not a new authorization
system.

### 6. Current consumers keep their own shape

Downstream consumers remain thin adapters over the canonical bus:

- `kb events` stays the bounded snapshot reader for history inspection.
- `kanban serve` may use the watch cursor to decide when to refresh the
  rendered page, but it must continue to serve canonical HTML from the store.
- MCP or plugin adapters that need live notifications should bridge the watch
  stream into their own transport instead of inventing another state store.
- Future local daemons may reuse the same process, but they are consumers, not
  publishers.

The important rule is that pub/sub invalidates or replays; it does not become a
second ledger or a third write path.

### 7. Durable subscriptions are declarative board records

`kb subscription add|list|show|pause|resume` manages the durable control-plane
record for one board. The addressed board database is the tenancy predicate;
there is no stored root, board path, or cross-board fan-in. Board schema v21
stores only:

- protocol version `1` and an immutable `sub-*` identity;
- an optional `task:ID` subject plus normalized `parent`, `ancestor`, and
  `depends-on` relations;
- normalized event-kind, prior/current-status, and registered-tag predicates;
- a strict named consumer and action identifier for later allow-list resolution;
- bounded timeout, retry, rate, and concurrency policy; and
- an optional opaque secret-reference identifier.

The row never stores shell text, executable arguments, roots, credentials, raw
tokens, or a caller-controlled adapter command. A later dispatcher must resolve
the consumer and action through host-local trusted configuration and must treat
the secret reference only as a lookup name. Adding, pausing, and resuming a
subscription append `subscription_added`, `subscription_paused`, or
`subscription_resumed` to the existing hash-chained board ledger. Those audit
payloads omit even the opaque secret reference.

Unknown subjects, historical relation targets, event kinds, statuses, or tags
fail closed at creation. Pause and resume are idempotent state transitions; no
update or delete surface can silently retarget an existing identity.
`subscription list` and `subscription show` are read-only request-response
operations and remain in generated MCP schemas. Delivery state and adapter
execution are explicitly deferred to the dispatcher phase.

The immutable delivery identity is `(subscriptionID, eventID)`. The
subscription protocol version, watch envelope version, stored event semantic
schema version, and binary release version are independent compatibility
numbers and must not be inferred from one another.

## Consequences

The repository gets one canonical live-history contract for both boards and the
registry. A watcher can resume from a durable cursor, and callers no longer need
to infer freshness from file state or duplicate the ledger logic in each
adapter.

The tradeoff is that every live consumer must persist and manage its own
cursor. That is deliberate: the cursor belongs to the consumer, while the
sequence belongs to the ledger.

This decision also keeps the current snapshot commands useful. `kb events`,
`kb context`, `kanban serve`, and any future adapter can all share the same
history source without sharing a mutable cache.

Cursor ownership stays with the consumer: the server does not remember a
watcher’s position, and the caller must persist and replay the last emitted
cursor itself.

## Rejected alternatives

- Polling `ledger_revision()` or a filesystem fingerprint on the DB, WAL, and
  journal files. That tells a consumer that something changed, but not what
  changed, where to resume, or whether the consumer has already processed the
  relevant event.
- Adding a separate broker or queue. That would create a second durable system
  with its own retention, failure, and tenancy rules, while the canonical event
  ledger already exists.
- Driving notifications directly from write transactions. That couples writes to
  slow readers and makes a read-side outage or backpressure problem a write-side
  problem.
- Building pub/sub around in-process callbacks or shared memory. That breaks the
  process separation this repository already enforces for long-running surfaces.
- Treating the server-rendered web UI as the bus. The UI is a consumer of the
  ledger, not the ledger itself.

## Rollout

The watch-stream phase did not require a schema migration: it adopted the
existing append-only event trails. The durable subscription phase adds board
schema v21 for declarative records while continuing to audit every lifecycle
mutation through that same board event trail.

The implementation rollout should be staged:

1. keep `kb watch` wired into CLI help, parser, `COMMANDS`, and `LONG_RUNNING`
   while keeping it out of generated request-response tool schemas;
2. keep ascending cursor query primitives for board and registry trails as the
   watch backend rather than reversing the bounded newest-first readers;
3. teach adapters to bridge the NDJSON stream rather than poll for refresh;
4. keep the current `/live` filesystem-fingerprint notice as compatibility
   behavior while the cursor-driven replacement stays in place, without
   changing the canonical rendered page or the interaction guards that protect
   in-progress typing;
5. preserve `kb events` as the bounded historical snapshot command;
6. keep the stream shape stable once published.
7. add board-local subscription records and command-schema projections before
   any dispatcher or adapter can execute them.

## Tests and operations

Release evidence should come from separate compiled processes, per
[ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md), and should prove
all of the following. The watch acceptance slice is coverage-driven, not
count-driven:

- replay from cursor `0` returns the full trail in ascending `seq` order;
- resuming from a saved cursor returns only newer rows;
- the board and registry scopes never cross;
- a resumed cursor that presents the wrong scope, selector, or normalized
  predicate set is rejected rather than replayed under the wrong identity;
- the follow loop reads one committed snapshot at a time and only observes
  rows after the read transaction that can see them has started;
- archived rows appear only when explicitly requested;
- the NDJSON stream is parseable as stdout output and each envelope carries the
  stable protocol `version`, `scope`, `cursor`, `type`, and `payload`;
- the payload is additive protocol-v1, legacy rows emit explicit null semantic
  fields, and `_semanticV1` never appears on stdout;
- removed subjects and historical relation targets remain replayable;
- incompatible envelope versions fail closed;
- idle periods produce heartbeats;
- and a slow consumer does not force writers to block or share a mutable queue
  with them.

The durable-subscription acceptance slice additionally proves:

- migration to schema v21 through a compiled process;
- add/list/show/pause/resume across separate compiled invocations;
- fail-closed selectors, policy bounds, identifiers, and secret references;
- one-board isolation and immutable subscription identity;
- lifecycle events on the existing audited watch stream with no secret
  reference in their payload; and
- accurate read-only/list-valued command-schema metadata.

Operationally, `kb audit verify` remains the integrity gate for chain health,
while `kb events` and `kb watch` serve different read patterns:

- `kb events` for bounded inspection;
- `kb watch` for live subscription and replay.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md)
- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md)
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md)
- [ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md)
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md)
- [ADR-021](ADR-021-settled-history-leaves-operational-indexes.md)
