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
server-rendered page again. That path is a compatibility invalidation signal
for the current UI, not a durable cursor stream, and it does not name the event
that caused a refresh.

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
bound to exactly one scope at a time.

The monotonic cursor is the ledger sequence number:

- board events use `events.seq`;
- registry events use `rule_events.seq`.

Consumers persist that integer and resume from the next value after the last
delivered record. Wall clock time, file mtime, WAL fingerprints, and payload
hashes are not cursors.

The resume identity is broader than the bare cursor. A resumed watch must carry
the stable addressed board or registry identity together with the selector kind
and selector value that produced the stream. A cursor reused with a different
scope or a different selector must fail closed rather than silently replaying
the wrong trail.

### 2. `kb watch` is a process, not an RPC tool

`kb watch` does not exist yet. Implementing this ADR means adding it to the CLI
help text, parser, `COMMANDS`, and `LONG_RUNNING` handling while keeping it out
of the request-response generated tool schemas the same way the existing
long-running surfaces are filtered out.

`kb watch` is the canonical long-running subscription command once it lands. It
is read-only, process-separated, and excluded from generated tool surfaces the
same way `mcp` and `serve` are. Downstream adapters that want a subscription
must spawn it or bridge it, not call it as an in-process function.

The contract is:

```text
kb watch [--task ID | --rule ID | --registry]
         [--kind KIND]
         [--cursor N]
         [--follow]
         [--all]
         [--limit N]
         [--json]
```

Semantics:

- With no selector flags, `kb watch` follows the addressed board's full
  `events` trail, matching the current `kb events` history view.
- `--task` narrows that addressed board trail to the selected task ID.
- `--rule` narrows the registry `rule_events` trail to the selected rule ID.
- `--registry` selects the explicit unfiltered registry trail.
- These selector forms are mutually exclusive and they never imply a different
  scope behind the caller's back.
- `--cursor` is the durable resume point; the watcher replays rows with
  sequence numbers greater than that cursor and never writes a cursor itself.
- `--follow` keeps the process open after replaying the backlog and emits new
  events as they are committed.
- `--limit` bounds replay work for a bounded bootstrap or test fixture.
- `--all` includes archived history where the underlying trail supports it.
- `--json` is the machine contract. The stream stays JSON on stdout; human
  diagnostics belong on stderr.

Each delivery is an NDJSON envelope containing:

- `version`, the stable pubsub protocol version, starting at integer `1`;
- `scope`, naming the addressed board or registry trail;
- `cursor`, the last delivered monotonic sequence number;
- `type`, a stable tag such as `event`, `snapshot`, or `heartbeat`;
- and `payload`, the underlying event JSON shape the current CLI already uses.

Envelope readers must reject incompatible protocol versions and fail closed.
The wire version does not track the changing binary release string.

The event payload itself stays compatible with the existing JSON representation:
`seq`, `taskID` where applicable, `kind`, `actor`, `payload`, `createdAt`,
`archived`, `prevHash`, and `eventHash` remain the fields downstream code sees.

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

This ADR does not require a schema migration. It adopts the existing append-only
event trails and formalizes their cursor semantics.

The implementation rollout should be staged:

1. land `kb watch` as a read-only process over the existing event tables, and
   wire it into CLI help, parser, `COMMANDS`, and `LONG_RUNNING` while keeping
   it out of generated request-response tool schemas;
2. add ascending cursor query primitives for board and registry trails, then
   make `kb watch` consume those primitives directly rather than reversing the
   bounded newest-first readers;
3. teach adapters to bridge the NDJSON stream rather than poll for refresh;
4. keep the current `/live` filesystem-fingerprint notice as compatibility
   behavior until the cursor-driven replacement is ready, then swap the trigger
   without changing the canonical rendered page or the interaction guards that
   protect in-progress typing;
5. preserve `kb events` as the bounded historical snapshot command;
6. keep the stream shape stable once published.

## Tests and operations

Release evidence should come from separate compiled processes, per
[ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md), and should prove
all of the following:

- replay from cursor `0` returns the full trail in ascending `seq` order;
- resuming from a saved cursor returns only newer rows;
- the board and registry scopes never cross;
- a resumed cursor that presents the wrong scope or selector is rejected
  rather than replayed under the wrong identity;
- the follow loop reads one committed snapshot at a time and only observes
  rows after the read transaction that can see them has started;
- archived rows appear only when explicitly requested;
- the NDJSON stream is parseable as stdout output and each envelope carries the
  stable protocol `version`, `scope`, `cursor`, `type`, and `payload`;
- incompatible envelope versions fail closed;
- idle periods produce heartbeats;
- and a slow consumer does not force writers to block or share a mutable queue
  with them.

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
