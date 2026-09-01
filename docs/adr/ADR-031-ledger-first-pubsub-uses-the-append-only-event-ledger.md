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

### 7. Durable subscriptions began as declarative board records

The declaration phase introduced
`kb subscription add|list|show|pause|resume` as the durable control-plane
record for one board. The addressed board database is the tenancy predicate;
there is no stored root, board path, or cross-board fan-in. Board schema v21
stored only:

- protocol version `1` and an immutable `sub-*` identity;
- an optional `task:ID` subject plus normalized `parent`, `ancestor`, and
  `depends-on` relations;
- normalized event-kind, prior/current-status, and registered-tag predicates;
- a strict named consumer and action identifier for later allow-list resolution;
- bounded timeout, retry, rate, and concurrency policy; and
- an optional opaque secret-reference identifier.

The row never stores shell text, executable arguments, roots, credentials, raw
tokens, or a caller-controlled adapter command. That phase deliberately left a
later dispatcher to resolve consumer and action through host-local trusted
configuration and treat the secret reference only as a lookup name. Adding,
pausing, and resuming a subscription append `subscription_added`,
`subscription_paused`, or `subscription_resumed` to the existing hash-chained
board ledger. Those audit payloads omit even the opaque secret reference.

Unknown subjects, historical relation targets, event kinds, statuses, or tags
fail closed at creation. Pause and resume are idempotent state transitions; no
update or delete surface can silently retarget an existing identity.
`subscription list` and `subscription show` are read-only request-response
operations and remain in generated MCP schemas. Delivery state and adapter
execution were explicitly deferred from that declaration phase.

The immutable delivery identity is `(subscriptionID, eventID)`. The
subscription protocol version, watch envelope version, stored event semantic
schema version, and binary release version are independent compatibility
numbers and must not be inferred from one another.

### 8. The dispatcher is a separate capability-gated process

Board schema v22 adds durable materialization, delivery, lease, attempt, retry,
acknowledgement, and dead-letter state. The worker is the dedicated compiled
`kanban-dispatcher` binary, never an in-process callback:

```bash
kanban-dispatcher --project NAME [--consumer consumer.name] [--once] [--json]
kanban-dispatcher --workspace /registered/root [--once] [--json]
kanban-dispatcher --db /exact/board.db [--once] [--json]
```

Normal execution requires exactly one explicit selector. `--consumer`
restricts execution to one consumer identity, `--once` performs one scheduler
step, and `--json` emits the bounded report on stdout. Help and version return
without opening registry or board state. Conflicting explicit and environment
selectors fail closed.

The trusted protocol-v1 configuration is
`$KANBAN_DATA_DIR/dispatchers.json`. A representative shape is:

```json
{
  "version": 1,
  "consumers": {
    "consumer.name": {
      "capabilities": ["deliver"],
      "actions": {
        "send": {
          "capability": "deliver",
          "executable": "/absolute/path/to/adapter",
          "args": ["fixed-subcommand"]
        }
      },
      "secrets": {
        "token-ref": {
          "sourceEnv": "HOST_SECRET_NAME",
          "targetEnv": "ADAPTER_TOKEN"
        }
      }
    }
  }
}
```

The example names environment variables, not values. The private data root and
config refuse group/other access and symlinks. The executable must be an
absolute regular non-symlink with an execute bit and no group/other write bit.
Consumers, actions, capabilities, fixed arguments, and secret mappings come
only from this operator-owned file. Adapter execution starts with a cleared
environment and receives only the configured target secret variable when the
subscription names that opaque reference. The dispatcher writes the validated
protocol-v1 request to stdin, drains stdout and stderr concurrently with a 1 MiB
cap on each, validates the response against the exact request, and supervises
the adapter process group across timeout or cancellation. This is a host trust
boundary, not an OS sandbox.

Startup loads and validates configuration before the first materialization or
claim. Each scheduler step then materializes new matches, recovers expired
leases, selects one due candidate, and resolves its configured target before
lock contention. It takes a per-consumer host lock, reloads and revalidates the
configuration under that lock, and atomically claims the candidate. The
delivery lease lasts `timeoutMs + 30 seconds`; the adapter runs outside any
SQLite transaction. Success or failure finalizes only the exact
`(subscriptionID,eventID,leaseToken)` claim. Failures use deterministic bounded
exponential retry and become dead letters after the declared budget. A paused
subscription is ineligible at claim time, so pause/resume is rechecked even for
already materialized rows.

SIGINT and SIGTERM stop polling and terminate a running adapter process group
before the exact failure is recorded. If the dispatcher or host dies after the
adapter succeeds but before acknowledgement, lease recovery records
`lease_expired` and retries. That boundary is intentionally at-least-once: an
adapter must use `(subscriptionID,eventID)` as its idempotency key and must not
assume exactly-once side effects.

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
existing append-only event trails. The durable subscription declaration phase
added board schema v21. The dispatcher phase adds board schema v22 for durable
delivery state while continuing to audit subscription lifecycle through the
same board event trail.

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
   any dispatcher or adapter can execute them;
8. ship the compiled dispatcher, host-local capability allow-list, durable
   delivery scheduler, and process-separated fake-adapter proof.

The first Codex queue bridge sits behind that dispatcher contract. The board
record names consumer `codex.queue` and action `enqueue-turn`; the checked-in
adapter executable is `kanban-codex-queue-adapter`. Host-local
`dispatchers.json` also pins the installed Codex executable, the exact
thread/session target, the required installed version, and a fixed host-local
Codex state directory argument `--codex-home /root/.codex` with matching
`CODEX_HOME=/root/.codex`. The subscription row does not select an executable,
session, shell text, or arbitrary args. Every invocation probes the installed
Codex binary directly for the exact version and the
`codex queue --help` surface, then fails closed on drift. The adapter passes
only that fixed `CODEX_HOME` to child Codex processes. On HAX, direct ingress
is `/root/.local/bin/codex queue --thread UUID_OR_EXACT_SESSION_NAME --message
TEXT` for `codex-cli 0.150.1`. Live support for that bridge is a separate smoke
receipt and is not implied by the architecture alone.

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

The generic dispatcher acceptance slice additionally proves through the
compiled `kanban-dispatcher` and a separately compiled fake adapter process:

- help/version avoid database access and all three explicit selectors resolve;
- configuration fails before materialization or claim and one consumer filter
  cannot claim another consumer's work;
- the adapter inherits no ambient environment and receives only the configured
  secret target, while request/report/error paths do not reveal its value;
- success and stable exit, malformed, mismatch, overflow, timeout, and
  cancellation failures persist their exact attempt outcomes;
- two worker processes contend for one delivery but produce one claim and one
  adapter invocation;
- pause/resume and SIGTERM operate at real process boundaries; and
- a post-success/pre-ack process crash survives lease expiry, records
  `lease_expired`, invokes the adapter again, and then records `success`.

The Codex queue adapter-contract slice additionally proves through compiled-
process adapter-contract coverage with a fake Codex executable:

- the adapter boundary only: argv selection, fixed `CODEX_HOME`, environment
  clearing, unrelated child acknowledgement, and an adapter-derived response;
- focused unit coverage for exact installed-version and `codex queue --help`
  drift rejection, trusted path identity, and bounded input/output handling;
  and
- no claim that installed Codex support is live.

The separately named HAX live smoke receipt is the distinct runtime check for
installed Codex support. It must use a separately owned idle test session in a
disposable workspace, with no human driver session, no shared repository
mutation, no terminal input modification, and no `send-keys`; it verifies the
installed version, the exact ingress path, and one received queued message.

The second experimental and opt-in bridge is consumer `codex.app-server`,
action `start-readonly-turn`, and capability `start`, via
`kanban-codex-app-server-adapter`. The host allow-list binding is installed,
but this rollout enables no active declarative subscription. When the
dispatcher invokes it, the adapter still accepts a structured `AdapterRequest`
and returns `AdapterResponse`; that is the normal dispatcher path, not a
general subscription bypass. The host pins the installed Codex CLI `0.150.1`,
the canonical path, private
`CODEX_HOME`, private empty cwd, the `ClientRequest` hash
`efcd14b3433960c5e64a294e0071d48150429a603a5a18df536c84b76a902317`, the
combined v2 schema hash
`8cdccfc35582696d7141e7f916e0d5a664ab5b5e90b732f104284d2507f369f8`, and the
protocol timeout. Each turn clears child env except `CODEX_HOME`, probes
version/help, regenerates the schema with experimental API disabled in an
identity-pinned 0700 temp dir, verifies both hashes, and cleans only that
directory. The turn is read-only, approval never, stdio only, and any request,
tool-ish item, policy/identity/status/schema/size drift, malformed output,
stderr, timeout, wrong/extra/duplicate ack, or post-completion output fails
closed. Success stdout is only `AdapterResponse` after one accepted
`(subscriptionID,eventID)` completion. The acceptance matrix for this bridge
is separate: focused unit protocol/runtime coverage, compiled-process
adapter-contract coverage against a dependency-free fake Codex, and a distinct
HAX live smoke against installed Codex/model.

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
