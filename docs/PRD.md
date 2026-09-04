# Kanban product requirements

**Status:** Active
**Owner:** George
**Updated:** 2026-08-31

## Product statement

Kanban is George's private, stateful representation of the projects he is
working on. It gives humans and compatible agent harnesses one durable place to
discover work, coordinate atomic ownership, report progress, preserve evidence,
and transfer work to a replacement agent when context or tokens run low.

## Goals

1. Show active work and progress across multiple registered projects.
2. Let every worktree of a project share the same project board.
3. Let agents safely read and update tasks through narrow typed operations.
4. Make token-pressure handoffs cold-startable without chat history or
   `HANDOFF.md`.
5. Keep all operational state private to the operator and outside product
   repositories.
6. Preserve history and prevent concurrent ownership or partial transitions.
7. Replace atmux's embedded Kanban implementation and make atmux a consumer of
   this portable work-state engine.
8. Expose a cursor-native `kb watch` process over the append-only ledgers,
   with additive protocol-v1 event payloads and fail-closed cursor semantics,
   while keeping `kb events` as the newest-first snapshot reader and `/live`
   as compatibility invalidation for the served UI.
9. Persist board-local declarative subscriptions with immutable identity,
   fail-closed predicates, bounded consumer policy, and secret references only;
   execute them only through the separate capability-gated compiled dispatcher,
   with the first Codex queue bridge remaining a separate HAX-smoke-gated
   contract before it counts as live.

## Non-goals

- A shared issue tracker for coworkers, customers, or delivery teams.
- Committing the live board, TODO files, or handoff files into product repos.
- Arbitrary SQL access for agents.
- Cross-host replication in the first release.
- Replacing Git, source documentation, or external customer issue systems.

## Primary workflows

### Register a project and its worktrees

The operator initializes a board with an explicit name. A root is optional and
attached as a discovery hint, and additional worktree roots can join later.
Registration is explicit and reversible; no filesystem crawl is required.

### Plan and execute work

An agent lists project tasks or the personal cross-project view, claims eligible
work with an expiring lease, appends progress/evidence, and checkpoints after
meaningful turns. Dependencies and leases prevent unsafe parallel execution.

### Hand off under token pressure

Before an agent runs out of useful context, it creates a `token_pressure`
handoff with a summary, current intent, one concrete next action, blockers,
validations, and repository state. Kanban atomically persists that packet and
releases ownership. A replacement agent accepts the handoff, receives a new
lease, loads the context packet, and continues.

### Review the personal portfolio

The operator can request an aggregate view containing each registered project,
its worktree roots, active task counts, blocked work, pending handoffs, and
recent progress. Reads occur across project databases without merging their
write domains.

## Functional requirements

### Retrieval and agent context

- Search every durable work-knowledge source without requiring the caller to
  know its project first.
- Fuse exact identifier/text retrieval, SQLite full-text ranking, and private
  local semantic similarity through one implementation shared by CLI, MCP, and
  the served UI.
- Stream append-only board and registry ledger changes through `kb watch` with
  one scope per invocation, opaque cursor resume, additive protocol-v1
  payloads, a `--task` subject selector, repeatable `--kind`, `--relation`,
  `--prior-status`, `--current-status`, and `--tag` predicates, stdout NDJSON, stderr
  diagnostics, heartbeats for idle periods, and fail-closed unknown or
  mismatched selectors.
- Return source-backed citations and bounded snippets, never an uncited
  synthesized answer.
- Support project, source, status, tag, lane, time, and archive filters, while
  keeping board semantic watch predicates off the registry scope.
- Keep ordinary search read-only. Index repair and semantic-cache persistence
  must be explicit, audited operations covered by backup and doctor workflows.
- Manage subscription add/list/show/pause/resume through the compiled CLI and
  generated schema. Store only normalized selection predicates, named
  consumer/action identifiers, bounded timeout/retry/rate/concurrency policy,
  and an optional opaque secret reference. Store no root, path, shell text,
  executable argument, credential, raw token, cursor, or delivery state.
- Run delivery through `kanban-dispatcher` with exactly one explicit board
  selector. Resolve executable, fixed arguments, capability allow-list, and
  secret environment mapping only from private host-local configuration;
  materialize before claim, serialize per consumer, invoke outside the SQLite
  transaction, and acknowledge or fail only the exact lease token.
- For the first Codex queue bridge, host-local `dispatchers.json` binds
  consumer `codex.queue` and action `enqueue-turn` to the checked-in
  `kanban-codex-queue-adapter`, the installed Codex executable, the exact
  thread/session target, the required installed version, and the fixed
  `--codex-home /root/.codex` state directory or the implementation's exact
  spelling. The subscription row never chooses executable, session, shell
  text, or arbitrary args. Each invocation probes the installed Codex binary
  directly for the exact version and `codex queue --help`, fails closed on
  drift, starts with an empty environment, and passes only that fixed
  `CODEX_HOME` to child Codex processes.
- For the second experimental and opt-in bridge, host-local `dispatchers.json`
  binds consumer `codex.app-server`, action `start-readonly-turn`, and
  capability `start` to the checked-in `kanban-codex-app-server-adapter`.
  The host allow-list binding is installed, but this rollout enables no active
  declarative subscription. When the dispatcher invokes it, the adapter still
  accepts a structured `AdapterRequest` and returns `AdapterResponse`; that is
  the normal dispatcher path, not a general subscription bypass. Private host
  config pins the canonical Codex path, private `CODEX_HOME`, private
  empty cwd, exact Codex CLI `0.150.1`, the
  `ClientRequest` hash
  `efcd14b3433960c5e64a294e0071d48150429a603a5a18df536c84b76a902317`, the
  combined v2 schema hash
  `8cdccfc35582696d7141e7f916e0d5a664ab5b5e90b732f104284d2507f369f8`, and
  the protocol timeout. Each invocation clears child environment except
  `CODEX_HOME`, probes version/help, generates the schema with experimental
  API disabled in its own identity-pinned 0700 temp dir, verifies both hashes,
  and removes only that directory. The turn is read-only (`{type:readOnly,
  networkAccess:false}`), approval is never requested, the instruction set
  forbids tools/files/network/commands and terminal/tmux/send-keys, and seven
  ambient/reasoning notification classes are explicitly opted out. Server
  requests, unconsumed notifications, tool-ish items, policy/identity/status/
  schema/size drift, malformed output, stderr, timeout, wrong/extra/duplicate
  ack, errors, and post-completion output fail closed. Success stdout is only
  `AdapterResponse` after exactly one `{accepted:true,idempotencyKey:<subscriptionID:eventID>}`
  completion. Delivery remains at-least-once.
- For the third opt-in bridge, host-local `dispatchers.json` binds consumer
  `claude.print`, action `start-readonly-turn`, and capability `start` to
  `kanban-claude-print-adapter`; no active declarative subscription ships.
  Private configuration pins canonical Claude, private `HOME`, private cwd,
  and an exact required version (HAX stable: `2.1.236`). The adapter validates
  each identity and ancestor chain before every spawn, probes exact
  `claude --version` and required help markers, and starts a fresh worker
  rather than resuming a foreground session. Child argv is fixed to safe print
  mode with JSON output, no tools or MCP, no session persistence, and
  `dontAsk`; child environment is exactly `HOME` plus `PATH=/usr/bin:/bin`,
  cwd is fixed, and stdin is empty. The prompt contains only bounded
  subscription/event IDs and static acknowledgement text. Success requires a
  strict JSON object or array whose final result object has the exact
  acknowledgement and no error or tool-use evidence. Nonzero status, stderr,
  API/auth errors, overflow, trailing JSON, and mismatches fail closed.
  Success stdout is only `AdapterResponse`. Compiled adapter-contract tests
  against the dependency-free fake Claude do not establish installed-Claude
  support; that requires a distinct live smoke.
  Live authentication is currently blocked by revoked OAuth attention
  `a-347ff24c`; the adapter must not work around authentication.

### P0 — first usable slice

- One SQLite board per project and one private SQLite registry.
- WAL mode, foreign keys, busy timeout, append-only migrations, and atomic
  ownership transitions.
- Attach multiple worktree roots to the same project board.
- Aggregate registered projects and task/handoff counts.
- Create, list, and accept structured agent handoffs.
- Include handoffs in cold-start context.
- Preserve the existing task, dependency, claim, note, checkpoint, and TODO
  projection contracts.
- Rust-only production CLI and domain engine, with Cargo-based development and
  compiled-binary end-to-end tests.

### P1

- atmux parity for task assignment, lanes, dependency updates, driver-only
  gates, stories, epics, readiness, sign-off, dispatch/inbox integration,
  hygiene checks, and cockpit projections.
- Importer for atmux `state.db` and legacy `kanban.json`, preserving stable
  identities, relationships, timestamps, notes, and extension fields, with an
  explicit atomic reconcile mode for stopped-writer cutover refreshes.
- Compatibility adapter followed by the verified removal of duplicate atmux
  Kanban storage and repository code.
- Operator-oriented terminal or web board over the same APIs.
- Search/filter by project, worktree, status, priority, assignee, and recency.
- Safe project/worktree detach and rename operations.
- Backup, restore, integrity-check, and retention commands.
- Harness adapters that automatically checkpoint and initiate handoff before a
  configured token threshold.

### P2

- Optional encrypted synchronization between the operator's own hosts.
- Notifications for stale claims, blocked work, and unaccepted handoffs.

## Handoff acceptance criteria

A handoff is valid only when:

- the outgoing agent owns a live lease for the task;
- summary, intent, and next action are non-empty;
- its checkpoint and lease release commit in the same transaction;
- the pending record is visible in task context;
- acceptance creates a different live lease and records the incoming agent;
- if the task is blocked, done, or cancelled, acceptance instead records that
  the brief was absorbed without minting a lease or changing task status;
- a named target cannot be accepted by a different agent; and
- a stale outgoing token cannot mutate the task after handoff.

## Privacy and safety requirements

- Default storage is `${XDG_DATA_HOME:-~/.local/share}/kanban`.
- Live state is ignored by and physically separate from managed repositories.
- No network listener, telemetry, publication, or remote synchronization is
  enabled by default.
- Secrets and lease tokens do not appear in notes, checkpoints, handoffs, or
  rendered model context.
- Every mutation has an actor and an auditable event.
- Subscription lifecycle audit events omit even the opaque secret reference;
  executable capabilities remain in host-local trusted configuration.

## Success measures

- A task created from one worktree is immediately visible from another attached
  worktree.
- Two processes cannot simultaneously own the same task.
- An outgoing and incoming agent can complete a handoff using only Kanban and
  the repository checkout.
- A saved watch cursor resumes the same ledger scope after restart and rejects
  mismatched or future cursors. Removed subjects and historical relation
  targets remain replayable.
- A subscription created on one board is invisible on every other board;
  pause/resume preserves its immutable ID and `(subscriptionID,eventID)` is the
  dispatcher's delivery identity.
- Two dispatcher processes can contend for one delivery but only one adapter
  invocation succeeds; a post-success/pre-ack crash is retried after lease
  expiry and is truthfully treated as at-least-once delivery.
- The Codex queue compiled-process adapter-contract coverage with a fake
  Codex executable proves only the bridge boundary: argv selection, fixed
  `CODEX_HOME`, environment clearing, unrelated child acknowledgement, and an
  adapter-derived response. Focused unit tests prove version/help drift
  rejection and bounded input/output rendering.
- A separately named HAX live smoke receipt against a separately owned idle
  test session in a disposable workspace proves installed Codex support, exact
  ingress, and one received queued message.
- The aggregate view accurately reports all explicitly registered projects.
- Process restart and database reopen preserve all task and handoff state.

## Delivery slices

1. **Foundation (current):** worktree aliases, aggregate project summary,
   transactional handoffs, CLI/API, context, and tests.
2. **atmux parity:** port its mature task/story/epic behavior and build import
   plus compatibility contracts.
3. **atmux cutover:** migrate state, switch every consumer, verify receipts,
   and remove the duplicate implementation with rollback retained.
4. **Operational hardening:** integrity/backup commands and automatic harness
   token-threshold integration.
5. **Personal UI:** fast portfolio and project board views.
6. **Optional mobility:** encrypted operator-only cross-host transport.
7. **Durable dispatch:** board-local declarations plus the separate compiled,
   capability-gated dispatcher, the Codex queue compiled-process
   adapter-contract coverage, and the separately named HAX live smoke receipt
   for installed Codex support.

## Current delivery status

The dated 2026-08-16 fleet preparation receipt records seven non-empty legacy
project sources imported into private boards without activation, producing a
3,042-row preparation snapshot. Those counts are historical evidence, not a
statement of current fleet health. Authority cutover remains gated on durable
Kanban handoffs for live agents, stopped writers, per-project activation
receipts, consumer restart verification, and a no-legacy-write observation
period.

The production runtime is Rust per ADR-006. A release is ready only when the
compiled executable passes the current process-boundary E2E matrix and the
focused atmux CLI adapter contract suite against current data. TypeScript and
Bun are not production or development dependencies of Kanban.

See [the historical 2026-08-16 fleet preparation receipt](migrations/atmux-fleet-preparation-2026-08-16.md).
