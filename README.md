# kanban

Durable, local-first work state for long-horizon agents and swarms.

Kanban is an operator-private personal work system across projects and Git
worktrees. It is not a shared team issue tracker.

Kanban is not primarily a board UI. It is an **agent-work ledger** designed
around one failure condition: an agent may disappear at any token boundary,
and a replacement must be able to resume safely without conversation history.

SQLite is authoritative. Plans, progress, evidence, decisions, claims, and
checkpoints survive sessions and process restarts. A generated `TODO.md` is a
human-readable projection, not a competing state store.

## Why

Chat history is volatile coordination state. It becomes incomplete during
context compaction, provider failure, rate limiting, model swaps, and swarm
fan-out. Kanban moves the minimum safe handoff contract out of chat:

- atomic task claims with expiring leases;
- dependency-aware pull scheduling;
- append-only plan, progress, blocker, decision, and evidence notes;
- structured checkpoints containing intent, next action, validations, and
  repository state;
- transactional token-pressure handoffs between outgoing and replacement
  agents;
- bounded cold-start context for the next model turn;
- workspace state outside managed product repositories.

## Quick start

Requires a Rust toolchain with Cargo.

```bash
cargo install --path . --locked   # installs both `kanban` and `kb`

cd /path/to/project
kanban init --name my-project
kanban task add "Implement durable resume" --id t-resume --priority P1

kanban claim t-resume --as deepseek --session turn-1 --json
# Save the returned leaseToken.

kanban note t-resume "Inspect persistence and restart behavior" \
  --as deepseek --kind plan

kanban checkpoint t-resume \
  --lease "$LEASE_TOKEN" \
  --as deepseek \
  --session turn-1 \
  --summary "Mapped the current persistence boundary" \
  --intent "Implement restart-safe checkpoints before orchestration" \
  --next-action "Add the recovery test" \
  --validation "schema migration passes" \
  --repo "$PWD" --branch main --head abc123

# A fresh agent needs only this plus the repository:
kanban context t-resume
```

Use `kanban claim --next --as <agent>` for pull-based swarms. A second worker
cannot claim the same task while its lease is live. If a worker disappears,
the lease expires and the task becomes claimable again; the last durable
checkpoint remains available.

Schedulers that need to inspect before dispatching use the same predicate
without taking a lease:

```bash
kanban claim --candidates --project my-project --as atmux@_superbot \
  --lane be --tag queuer --limit 20 --json
```

The result is priority ordered and contains task fields, including tags, lane,
assignee and `driverOnly`, but never a lease token. It excludes containers,
work behind an unmet completion gate, work under draft plans, active leases,
incompatible assignees and driver-only work unless `--caller-scope driver` is
supplied. Inspection is read-only: it does not migrate or touch registry
recency, expire leases, update task state, append events, or cache a result. A
returned row is still only a candidate; take it with atomic `claim ID` or
`claim --next`.

To ask who holds a task, read the lease, not the assignee. Every `task list`
row carries `claimed`, and `task list --with-claims` (or `task show`) adds
`claim`, whose holder is `claim.agentID` alongside `expiresAt`; a free task has
`claimed: false` and `claim: null`. There is no `claim.actor`, and `assignee`
is a separate field recording intent, not possession: an assigned task can be
free, and a held task can be assigned to someone else. `--fields actor` is
refused naming the keys that exist, so the wrong question fails where it is
typed rather than answering null.

## Completion gates

A dependency is a gate on doing the work, not only on being offered it. Declare
one with the dependency flags that already exist — `task add --depends-on ID`,
`task update --depends-on ID ...` (a repeatable set replacement) and
`task update --clear-dependencies` — and a row inherits every prerequisite
declared on itself **or on any ancestor**, the same way a draft ancestor holds
back its whole tree (ADR-013). A prerequisite satisfies the gate at `done` and
nowhere else: `cancelled` is a decision not to do the work, and an archived
`done` row is still finished.

While a prerequisite is unfinished, these refuse, naming the row, each
unfinished prerequisite with its status, and the ancestor that declared it:
`claim ID`, `claim --next`, `handoff accept`, `task add`/`task move` into
`in_progress`, `review` or `done`, `story advance` past `ready`,
`checkpoint --state continue|done`, and `heartbeat`. `--force` seizes a lease;
it does not finish a prerequisite, so it is not a way through.

What stays open is everything that records where the work stands rather than
claiming it moved: `task move` to `draft`, `backlog`, `todo`, `blocked` or
`cancelled`, `note`, `sitrep`, `attention`, `handoff create`,
`checkpoint --state blocked` and `release`. A live lease is never revoked by a
gate — a prerequisite introduced or reopened mid-lease stops the next renewal
and leaves the holder the blocked checkpoint and the release. Nothing here
auto-changes a status, claims work, marks an epic done, or reopens finished
work.

The blockers are data, not only a refusal. `task show`, `kanban context` and
`task list --with-relations` carry `blockingGates`: an array of
`{sourceTaskID, prerequisiteID, prerequisiteTitle, prerequisiteStatus}`,
ordered nearest owner first then by prerequisite id, where `sourceTaskID` is the
row that declared the edge — itself or an ancestor. `dependencies` is unchanged
and still lists only what the row declared, so `--fields blockingGates` needs
`--with-relations` exactly as `--fields dependencies` does. An empty array means
no dependency gate; it is not a promise that the row is claimable, because
draft ancestors, live leases, routing and authorization are separate rules.

A dependency that no amount of work could satisfy is refused when it is
declared, on the dependency change and on a parent change alike: an epic gated
on a task inside its own subtree can never be unblocked, because the descendant
inherits the epic's gate and would wait on itself. The existing self-dependency
and dependency-cycle refusals are unchanged.

```bash
kb t up "$LEAF" --depends-on "$PREREQ" --as "$AGENT" --json
kb t cat "$LEAF" --json | jq .blockingGates
kb claim "$LEAF" --as "$AGENT"      # refused, naming PREREQ and its owner
kb t mv "$PREREQ" done --as "$AGENT"
kb claim "$LEAF" --as "$AGENT"      # granted
```

## Roadmap todo lists

Use the board tree for every durable multi-item todo list. The roadmap is an
epic, each top-level todo item is a direct child epic, and its stories and tasks
are the work. Dependencies between child epics express ordering.

```bash
kb t new "Q4 roadmap" --type epic --status draft --body-file roadmap.md --tag planning --as "$AGENT" --json
# Use the returned epic id as ROADMAP_ID.
kb t new "Migrate billing" --type epic --parent "$ROADMAP_ID" --status draft --tag billing --as "$AGENT" --json
# Use the returned child id as ITEM_ID.
kb t new "Enumerate billing consumers" --parent "$ITEM_ID" --tag billing --as "$AGENT" --json
kb t mv "$ROADMAP_ID" todo --as "$AGENT"
```

The tree and its statuses are authoritative. Keep scope and success criteria in
the roadmap body, but do not maintain a second Markdown checkbox list. Any
checklist view is a projection. A child epic may be marked done only after every
non-cancelled descendant is settled and its completion evidence is durable;
that check is agent-verified today, not automatically derived by the CLI. A
single standalone action remains a task and needs no epic wrapper. See
[ADR-022](docs/adr/ADR-022-roadmap-todo-lists-are-child-epics.md).

## Long-horizon loop

An orchestrator should run fresh, bounded model turns instead of depending on
one ever-growing conversation:

```text
claim task
  -> load `kanban context`
  -> work for one bounded turn
  -> append notes and write a checkpoint
  -> CONTINUE | BLOCKED | DONE
  -> start another fresh turn when CONTINUE
```

`blocked` and `done` checkpoints atomically update the task and release its
lease. A `continue` checkpoint retains the lease.

When a model turn is ending, transfer ownership through Kanban itself:

```bash
kanban handoff create t-resume \
  --lease "$LEASE_TOKEN" --as outgoing-agent --reason token_pressure \
  --summary "Implemented the schema" \
  --intent "Keep the migration append-only" \
  --next-action "Run the importer contract test"

kanban handoff list --status pending --json
kanban handoff accept h-12345678 --as incoming-agent --json
kanban context t-resume
```

## Storage and privacy

`kanban init` registers the current workspace in an operator-private registry:

```text
${XDG_DATA_HOME:-~/.local/share}/kanban/
  registry.db
  boards/<uuid>.db
```

No state is written into the managed repository. Override discovery with
`KANBAN_DATA_DIR`, `KANBAN_DB`, or `--db PATH`. This permits atmux to retain
per-team databases while orch and other harnesses share the same API.

Concurrent writers queue rather than fail. Every transaction is
`BEGIN IMMEDIATE`, and a board that is already being written is retried with
randomized backoff for fifteen seconds before the command gives up
([ADR-009](docs/adr/ADR-009-swarm-write-contention.md)). Measured on a
sixteen-agent fan-out, that took the failure rate from 3% to zero and the
slowest write from 6.7s to 4.5s: an agent reads an exit status and moves on, so
a dropped write is lost work nothing downstream will notice is missing.

## Tag-scoped `/kb` rules

Short, non-secret constraints live in one registry-owned rules document. Boards
still own work; they are selectors on rules, never a second rule species:

```bash
kb r new "Universal rule." --as geoyws                    # tags: ALL
kb r new --body-file /tmp/non-secret-rule.md --as geoyws
kb r ls                         # active table of contents, oldest first
kb r cat r-12345678             # fetch one full body lazily
kb r up r-12345678 --body "Production runtime is compiled Rust." --as geoyws
kb rule retire r-12345678 --as geoyws
kb r ls --all --full            # include retired rules and full bodies
kb ev --rule r-12345678         # audited revision/retirement trail

# Target one or more boards, or every board except named boards.
kb r new "Kanban-only rule." --board kanban --as geoyws
kb r new "Everywhere except project-a." --except-board project-a --as geoyws

# Intersect board selection with one or more subsystem tags.
kb r new "Queuer-specific rule." --tag queuer --as geoyws
kb r new "Aix rule on two boards." --board crm-react --board pai-root --tag aix --as geoyws
kb r up r-12345678 --clear-tags --as geoyws
```

Examples below use bare tag names in storage and CLI. Prose may render them as
`:slug`. `@:team` stays atmux routing identity, not a tag.

The first line is the headline. Every context packet, newly granted claim and
accepted handoff carries the applicable active table of contents. Each compact
summary has one `tags` array plus id, headline, byte size and whether more body
exists; `kb r cat ID` fetches long bodies lazily. Other commands do not repeat
rules, and a stored claim does not pretend it re-read current rules.

`ALL` is the default selector. Named includes are `ONLY:<board>`; exclusions
are `ALL, EXCEPT:<board>`. Operators use repeatable `--board` and
`--except-board`, and the CLI validates exact registered board names. Lowercase
`--tag` values are subsystem selectors registered on at least one active board.
An active `ONLY:<board>` or `EXCEPT:<board>` reference is valid only while
exactly one active board has that name. Rule add, active-rule update, and rule
import recheck this invariant inside the write transaction. Workspace retirement
refuses any active rule naming the board and lists the blocking rule IDs; update
or retire those rules, then retry retirement. Retired rule bodies, tags, and
events remain available through `rule list --all`, `rule show`, and rule events.
`doctor --json` reports the complete check as
`activeRuleSelectors: { healthy, errors }`; stale legacy or manually edited rows
make top-level health false without blocking recovery reads, and `doctor --all`
continues to inspect retired boards.
Several subsystem tags are an OR set, intersected with the board selector.
Task claim, context and task-handoff injection require a matching task tag;
taskless session handoffs and web board projections omit subsystem-scoped rules.
See [ADR-027](docs/adr/ADR-027-rules-are-one-tag-scoped-kb-document.md).

Rules are retire-only and audited: updating records the prior body, retirement
removes a rule from active contexts without deleting it, and there is no `rm`
alias. They do not replace private memory. Long-form context, cross-machine
knowledge and anything secret remain in versioned/git-crypt'd dotfiles. Never
put credentials or secret values in the plaintext board database. See
[ADR-018](docs/adr/ADR-018-project-rules-frame-work-without-replacing-private-memory.md)
and its superseding [ADR-027](docs/adr/ADR-027-rules-are-one-tag-scoped-kb-document.md).

An attention item is a **decision card**: a question, the context needed to
answer it, and two to four authored choices, each with the consequence of
picking it and a machine-readable `outcome` of `approve`, `reject`, `defer` or
`other`, exactly one marked as the recommendation. Every item also offers an
implicit `custom` answer that needs its own `--outcome` and a note, so nothing
closes an item without a verdict; a row that authored no choices is served as
the `approve`/`reject` default pair with no recommendation, and the body stays
what it always was — the long form
([ADR-042](docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md)).
Attention rows carry the same registered subsystem vocabulary as tasks:

```bash
kb att raise "Review the deployed queuer" --as codex@driver --kind review --tag queuer \
  --question "The queuer is deployed but unproven - review it now, or ship and review after?" \
  --context "The queuer has been live on staging since 2026-09-05 with no errors. One task waits on the review, and waiting costs a day of feedback." \
  --choice "review-now=Review the deployed queuer today|approve" \
  --consequence "review-now=You spend about twenty minutes reading it today and the waiting task unblocks this afternoon." \
  --choice "ship-first=Ship it and review after the release|defer" \
  --consequence "ship-first=The release goes out unreviewed and a task is filed to review it on 2026-09-12." \
  --recommend review-now
kb att list --status open --tag queuer
kb att update a-12345678 --body "Corrected request." --as codex@driver
kb att update a-12345678 --tag queuer --tag infra --as codex@driver
kb att update a-12345678 --clear-tags --as codex@driver
kb att update a-12345678 --clear-card --as codex@driver
kb att resolve a-12345678 --as geoyws --choice review-now
kb att resolve a-12345678 --as geoyws --choice custom --outcome defer --note "After the aix pin lands."
kb att reopen a-12345678 --as geoyws --note "Resolved the wrong item."
```

Several tags describe several touched subsystems. An agent may correct the body,
tags or card only while the item remains open; the prior body, tags and card
stay on the event trail, and the update does not settle the request. Resolving
it freezes the row with the rest of the historical receipt. Unknown tags are
refused rather than producing an empty-looking filter result. Every card refusal
names its fix — an undeclared `--consequence` key, a duplicate key, a count
outside two to four, no recommendation or two, a choice with no consequence,
half a question/context pair, the reserved `custom` key, a `--recommend` with no
choices, and each length bound — and refusals are store-level, so the CLI, the
MCP tools and the web read the same wording.

Resolution is deliberately asymmetric: the operator actor `geoyws` may settle
any item, while an agent may settle only an item whose `raisedBy` is that exact
actor, and every resolution requires a `--choice`. `geoyws` is the one
operator spelling (`OPERATOR_ACTOR` in `rust/model.rs`); `geo` is not an alias
and is refused like any other non-raiser. Rows resolved before 2026-09-05 carry
`geo` in `resolvedBy` and `raisedBy` as the historical spelling; they are left
as recorded. Settling writes a `decision` of
`{choice, outcome, note, by, at}` on the row and into the `attention_resolved`
event, and composes `resolution` itself as `Decision: <label>. <consequence>`
plus a `Note: <note>` line when a note was given — one composer inside the
write path, so no caller can produce a different trail. A `--choice` naming a
key the row does not carry is refused by name, which is what makes a card a
browser is still holding safe. Rows settled before 2026-09-08 keep their exact
resolution bytes, including the `Comment: ` second line the web used to write.
If a resolution was mistaken, only `geoyws` or the recorded resolver may reopen
it. Reopening returns the item to the open queue without clearing `resolvedAt`,
`resolvedBy` or `resolution`; it clears `decision` from the row and keeps it in
the `attention_reopened` event, adds `reopenedAt`, `reopenedBy` and
`reopenNote`, and the transition is audited.

## Working from anywhere

Boards are addressable from any directory, not only from inside the project
tree. Every flag you type is consulted before any environment default, and
defaults are consulted before the working directory
([ADR-007](docs/adr/ADR-007-global-project-addressing.md),
[ADR-008](docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)):

| Order | Selector | Meaning |
| --- | --- | --- |
| 1 | `--db PATH` | a board file directly |
| 2 | `--project NAME` | a registered project by name, from anywhere |
| 3 | `--workspace PATH` | the project containing `PATH` |
| 4 | `KANBAN_DB` | the default for `--db` |
| 5 | `KANBAN_PROJECT` | the default for `--project` |
| 6 | _(none)_ | the project containing the working directory |

A flag beats an environment default, because a default is what applies when
nothing was asked for — with `KANBAN_DB` exported, `task list --project alpha`
reads alpha. Two flags are a different thing: `--project alpha --db /tmp/x.db`
is refused rather than resolved, because the values disagree and nothing in the
receipt would say which one was used.

A selector applies only to a command that resolves one board. `doctor`,
`dashboard`, `backup`, `restore`, `audit verify`, `serve`, `schema`, `mcp` and
the `workspace` and `rule` subcommands address the registry instead, so they
refuse a selector by name rather than accepting and discarding it:
`doctor --db PATH` used to answer `healthy: true` about every registered board,
which is the wrong answer in the one shape an operator believes. `init` and
`workspace attach` honour `--workspace`, which names a tree rather than a board,
and refuse the other two. `events --registry` and `events --rule` read the
registry trail and refuse all three, as `watch` already did.

Refusing is also what keeps the data-root lock honest. The lock decides its
scope from the board an invocation addresses, so while these commands accepted a
`--db` they ignored, that flag's only effect was to *suppress* the lock:
`KANBAN_DB=/tmp/elsewhere.db kanban restore --force` replaced the whole data
root with no exclusive lock at all. A command that discards a selector now locks
as though none were given.

```bash
kanban task list --project my-project          # from any directory
export KANBAN_PROJECT=my-project               # or once per shell or agent cage
kanban task add "ships from anywhere"
kanban task list --project other               # the flag still wins over the export
kanban workspace list                          # registered names, including rootless boards
```

A `--db` path that does not exist is created only by `task add`, and only when
you name the path yourself: `KANBAN_DB` pointing at nothing is a misconfigured
default, not a request to make a board, and every other command reports the
missing file rather than answering `[]` from one it just made.

A `--db` path that already holds a file is opened only if that file looks like
a board: a `tasks` table carrying kanban's own columns, and a schema version
that has been set. Opening a board migrates it, so this is what stops a mistyped
path from being rewritten — an empty file, a text file, a directory, a FIFO and
another application's SQLite database are all refused and left byte-for-byte as
they were.

Two limits worth stating rather than implying. It is a heuristic: a database
that happens to have a kanban-shaped `tasks` table would still be treated as a
board. And it only guards paths that already hold a file — typo `--db` onto a
path that does not exist and `task add` will start a board there, because that
is indistinguishable from asking for one.

`kanban schema --json` publishes this: each operation carries `readOnly` (writes
nothing anywhere), `createsBoard` (may bring a board file into existence) and
`ignoredSelectors` (the board selectors it refuses). The first two are different
questions — `todo` writes a file, so it is not read-only, and it still may not
create a board — and adapters can rely on all three: the MCP tool builder reads
`ignoredSelectors` so no tool offers an input the CLI would reject.

`kanban mcp` lists one further tool that is not an operation: `batch`, which
takes `{calls: [{name, arguments}]}` and answers up to 32 of them on one
request, in order, as `{results: [{ok, result} | {ok, error}]}`. It exists
because a remote agent loop pays a round trip per read — twelve reads, 2.45 s
p50 from the MBP to the board home over SSH, measured 2026-09-07 with payload
projection already in place — so the latency, not the payload, is the cost. It
is a transport shortcut and can do nothing a separate call could not: every
entry must name a tool whose operation is `readOnly`, the whole batch is
validated before any of it runs, so a batch naming one write performs none of
its reads, and each entry runs the same code path a standalone `tools/call`
runs, so a batched result is byte-identical to the same call made alone. A
failing entry travels as that entry's `error` beside the others' results rather
than sinking the batch. `claim --candidates` is read-only in the CLI but shares
a command row with the `claim` that writes a lease, so a batch refuses it and a
caller issues that one read on its own.

`kanban transact` is the writing half, and the thing it adds is not speed but
failure: it takes the same items, `(--items JSON_ARRAY | --items-file PATH)`
holding `[{"name": TOOL, "arguments": {…}}]`, runs up to 32 of them in order
against one open board inside one `BEGIN IMMEDIATE`, and either all of them
land or none of them do. It answers with one envelope — `{"ok", "batchId",
"failedIndex", "rolledBack", "results": [{"index", "ok", "result"} |
{"index", "ok": false, "error"} | {"index", "ok": false, "skipped": true}]}` —
and exits non-zero whenever `ok` is `false`. Execution stops at the first
failure, every item before it is rolled back, and every item after it is
`skipped` rather than attempted, so `rolledBack: true` means the board is
exactly where the batch found it. `rolledBack: false` on a failure means the
opposite and only one thing: the list was refused before anything ran, because
an item was malformed, named an operation a batch may not carry, named a second
board, or carried a back-reference that could not resolve. Any argument value
may be `{"$ref": {"item": N, "path": "/json/pointer"}}`, replaced by the value
at that RFC 6901 pointer inside item `N`'s result, where `N` is strictly
earlier — which is how the lease token a `claim` returns reaches the
`checkpoint` three items later without the agent handling it. Each item is
authorized exactly as if it had arrived alone, and each landed item's ledger
event carries `batchId` and `batchIndex`, so the order is reconstructable from
the ledger and a `batchId` there always means a batch that landed whole. Two
things a rollback does not undo, stated because they are invisible otherwise.
Opening the board retires expired claims and commits that sweep before any
batch scope exists, so a `transact` that rolled back may still have retired
somebody else's lapsed lease and appended those events — correct, because the
sweep is not part of the caller's batch, but it means a rolled-back `transact`
is not a no-op on the ledger in every possible sense. And there is no
idempotency key in V1: **a replayed batch is not a no-op.** A replayed `claim`
is refused, so a loop batch that starts with one fails at index 0 and lands
nothing — good, but good by accident of `claim`'s own semantics. Two replayed
`note` items are two notes. An agent that cannot tell whether a `transact` was
received must read the board rather than retry blindly. `kanban mcp` offers it
as the tool `transact`, taking `{"items": [{"name", "arguments"}]}` with
`readOnlyHint: false` and the board selectors on the call rather than on any
item: the server stages the list in a private temporary file it always removes,
runs the binary **once** for the whole batch — a process per item would be a
connection per item, and two connections cannot share a transaction — hands
back the envelope exactly as the CLI printed it, and marks the tool result an
error whenever `ok` is `false`; the read-only `batch` refuses `transact` as it
refuses every other write.

Project names are not unique. If two boards share one, `--project` refuses and
names every candidate, including rootless boards; use `--workspace PATH` or a
registered path to pick one. `workspace attach --to .` and similar path-like
inputs keep path resolution; bare names remain name-based.

Register a board by explicit name, then attach additional Git worktrees to it
when needed:

```bash
kanban init --name my-project --workspace /path/to/main-worktree
kanban init --name scratchboard --rootless
kanban workspace adopt --from-board /path/to/existing-board.db --name imported --workspace /path/to/adopted-root --as geoyws
cd /path/to/another-worktree
kanban workspace attach --to my-project
kanban workspace detach --root /path/to/retired-worktree --as geoyws
kanban workspace retire NAME --as ACTOR --note TEXT
kanban workspace unretire NAME --as ACTOR
```

If a name is reused and one candidate is rootless, `workspace attach --to NAME`
chooses that unique rootless board. The registry refuses to create a second
active board with the same name, and `workspace detach` refuses a last-root
retirement only when it would create a second active board with that name, so
the ambiguous state is blocked instead of becoming a dead end.

`workspace adopt` is for a board file that already exists outside the registry
and needs to become registry-owned storage. The source argument must identify
the exact regular board file: a symlink `--from-board` path and `..` parent
traversal are refused rather than silently changing source identity. Adoption
opens the source and any WAL with no-follow handles, verifies their device and
inode identities, and captures a stable WAL-aware snapshot without opening
SQLite on or creating a sidecar beside the source. Source integrity,
foreign-key integrity, audit, schema, and name preflight completes before the
live registry path or lock can be created.

Migration and final validation happen in a private staging directory. Kanban
hashes the pinned final database handle and atomically publishes that same inode
with a directory-relative rename into a freshly UUID-named file. Every
registry path component is opened without following symlinks, and the pinned
`boards/` identity is reverified before the registry transaction commits. The
receipt and immutable `board_adopted` event therefore describe the exact bytes
registered, not a later path reopen. Use `--rootless` when the adopted board
should have no registered root; otherwise pass the exact root you want recorded.

If a registered tree is later moved and a symlink left where it was, the root
row is now only a hint. `doctor` reports that stale root and where it leads
now; `kanban workspace repoint` points it at the tree's new home, repairing the
attached root without changing board identity.
When an attached worktree is intentionally gone, `workspace detach` retires
that root without deleting its registry history; `workspace list --all` shows
the detached row. The last root may be retired too, leaving a rootless board
that stays reachable by name.
kanban dashboard
```

SQLite runs in WAL mode with `synchronous=FULL`, foreign keys, and a five-second
busy timeout. Mutations use prepared statements and `BEGIN IMMEDIATE` where
ownership is decided. Agents should use the typed library or CLI; arbitrary
write SQL is not part of the public contract.

The registry directory is mode `0700`; database files and snapshots are created
mode `0600` before SQLite opens them, so they are never briefly world-readable.
Directories Kanban creates are `0700` from creation. Kanban never re-permissions
a directory it did not create, so pointing `--db` at a shared path leaves that
path alone ([ADR-008](docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)).
When the CLI runs as root inside a data directory owned by somebody else — the
shape on hax, where `kb` over ssh is root while `kanban serve` runs as the
`kanban` user — every file it creates or opens there, the `-wal` and `-shm`
beside a board included, is given that directory's owner, so a root command
cannot leave the service unable to open its own data.
Check and back up all registered boards with:

```bash
kanban version
kanban doctor
kanban audit verify --json
kanban backup --json
```

`version` prints the package version plus the newest board and registry schema
versions the binary supports. This makes two builds with the same package
version but different migration ladders visibly different before either one
opens a shared board. `doctor --json` also reports the registry's actual and
supported schema versions and, for every present board, its actual and
supported schema versions.

Board events and registry rule events are SHA-256 hash-chained. `audit verify`
walks every complete journal, including cold history, and exits non-zero on an
edited, deleted or reordered row. `doctor` includes the same check. Schema v18
for boards and v10 for the registry establish an explicit legacy boundary:
pre-migration rows are preserved and hashed, but are not represented as having
been protected before that boundary. See
[ADR-029](docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md).

## SQLite-native retrieval and RAG

Kanban indexes authoritative work rows inside each board database and searches
the registry-owned rules document alongside them. Search covers tasks, notes,
checkpoints, handoffs, attention, sitreps, rules and selected audit events.
Every hit names the source with a stable `kanban://BOARD/KIND/ID` or
`kanban://rules/rule/ID` citation; it does not synthesize an answer.

```bash
kb search "deploy the live build" --project kanban --json
kb search t-12345678 --source task --limit 5 --max-chars 4000 --json
kb search "login recovery" --tag auth --all-boards --json
kb search "old decision" --all --after 1780000000000 --json

# Explicit maintenance write: refresh every cached local semantic vector.
kb search-rebuild --project kanban --as system@search-index --json
kb search-rebuild --all-boards --as system@search-index --json
```

Ranking fuses exact identifiers and phrases, SQLite FTS5/BM25, and a small
deterministic local semantic model (`kanban-semantic-lite-v1`). The local model
keeps retrieval private and offline; it is a retrieval aid, not a general
embedding model. Missing or stale vector cache entries are computed in memory,
so `search` remains read-only. Incremental writes re-embed the documents they
touch in the same transaction, and `search-rebuild` refreshes the whole corpus
and writes an audit event. `--limit` and `--max-chars` bound agent context, and
`--source`, `--status`, `--tag`, `--lane`, `--after`, `--before`, and `--all`
filter it. `doctor` reports source/document/FTS parity plus cache freshness and
fails when any document lacks a current embedding.

The same command description generates the MCP `search` and `search_rebuild`
tools. The served UI exposes cross-board search at `/search`; both surfaces call
the same Rust retrieval implementation as the CLI. See
[ADR-023](docs/adr/ADR-023-sqlite-native-hybrid-rag-search.md) and the
[evaluation contract](docs/testing/rag-search-evaluation.md).

`doctor` reports per project and exits non-zero if anything is wrong. It runs
`integrity_check`, which validates the b-tree, and then the things a b-tree
check cannot see: rows whose foreign key points at something that is gone
(`orphanedRows`), tasks stamped in the future (`futureDatedTasks` — they sort
ahead of real work, and on a claim they hold a lease no sweep will retire), and
whether each registered board file is still on disk at all (`present`).

That last one matters more than it sounds. Opening a board creates it, so a
board file that has vanished used to be silently replaced with an empty one by
whatever touched it next — `doctor` included, which then reported the result
healthy. Commands that do work on one board now refuse and name the recovery;
`doctor`, `dashboard` and `backup` report the gap and carry on, and `backup`
lists what it could not include under `missingBoards`.

A board that is there and will not open is reported apart from one that is
gone, because the two call for opposite moves: you recover a missing board by
restoring a snapshot over its path, and doing that to an unreadable one
overwrites intact data. Boards are created `0600`, so one written by another
user is unreadable and perfectly healthy at the same time. `doctor` and
`dashboard` carry `boardState` — `readable`, `unreadable` or `missing` — with
the reason the read failed beside it; `backup`, `audit verify`, `search
--all-boards` and `search-rebuild --all-boards` carry an `unreadableBoards`
list alongside `missingBoards`. `doctor` and `audit verify` count an unreadable
board as unhealthy: nothing about it was checked, and a tool that could not
open a file cannot certify it.

Import an existing atmux board into the currently registered project without
writing to the source database:

```bash
kanban import atmux-sqlite /path/to/.atmux/state.db --as operator --json
```

Imports are insert-only by default. If a previously imported board must be
refreshed before cutover, stop every legacy writer and opt in explicitly:

```bash
kanban import atmux-sqlite /path/to/.atmux/state.db \
  --as operator --reconcile --json
```

Reconciliation atomically refreshes the imported records and relationships,
preserves durable history, and reports created versus updated counts. It is a
pre-cutover operation, not a dual-write mode.

Add `--dry-run` to any import to see the receipt it would produce without
writing: the whole import runs and is rolled back, so the preview comes from
the real code path rather than a second estimate of it.

```bash
kanban import atmux-sqlite /path/to/.atmux/state.db \
  --as operator --reconcile --dry-run --json
```

Reconciling over a task somebody is holding refuses by default and names every
holder, because overwriting it voids their lease. `--force` seizes them and
records a `lease_seized` event apiece, exactly as `task move --force` does, and
the receipt lists the tasks under `seizedLeases`.

Epics, stories, tasks, hierarchy, stable IDs, timestamps, routing fields,
notes, and unknown extension JSON are preserved. Dangling historical
relationships are retained as warning metadata rather than inserted as invalid
foreign-key edges.


## Sitreps

```bash
kanban sitrep post "Retry path is the culprit; fix is in the queuer." \
  --as claude@driver-2 --lane driver-2
kanban sitrep list --lane driver-2          # current view, newest first
kanban sitrep list --lane driver-2 --all    # including what it superseded
```

Keyed to a lane, needing no task and no lease — the low-ceremony sibling of a
handoff. A note requires a task; a checkpoint requires a task and a live lease;
so work done across tasks, between them, or before anything is claimed had
nowhere durable to go.

Posting archives everything past the newest ten in that lane. Archived sitreps
are hidden from the default read and returned by `--all`; **nothing is deleted**
— archiving bounds the view, not the table. Provenance (worktree, branch, HEAD,
root HEAD, dirty count) is captured rather than requested, and `context` carries
a task's sitreps so a resuming agent gets them without going looking. A
checkpoint, handoff, or sitrep written from outside a checkout is refused rather
than stored blank; `kb-board` supplies the checkout for you.

A task's `status` is a workflow state and always a `--status` flag; a *sitrep*
is prose about a lane and always the `sitrep` command. The old `status` command
is deliberately unknown rather than a deprecated alias. Reasoning:
`docs/adr/ADR-017-*`.

## Long-term archival

Settled ledger history leaves default reads and operational secondary indexes
after 90 days, while remaining inside the same SQLite board:

```bash
kb archive --older-than-days 90 --as system@archive --dry-run --json
kb archive --older-than-days 90 --as system@archive --json
kb task list --all --json
kb events --task <id> --all --json
```

The nightly backup job runs the sweep across every present registered board
before snapshotting. Nothing is deleted; `--all` is the explicit cold-history
read. See ADR-021.

Deployment attempts use the same retention path. The latest verified success
for each `(repo, tier, environment)` and every still-started attempt remain hot;
older terminal, non-current attempts self-archive during the nightly 90-day
sweep. They remain in SQLite, backups, audit history, `deploy list --all`, and
search with `--all`.

## Deployment ledger

```bash
kb deploy start --repo geoyws/kanban --commit "$FULL_SHA" \
  --tier @_p --environment production --host hax --url https://kb.geoy.ws \
  --task "$TASK_ID" --operation-id "$OPERATION_ID" --as codex@driver --json

kb deploy finish "$DEPLOYMENT_ID" --token "$CAPABILITY_TOKEN" \
  --result succeeded --phase verification --served-commit "$FULL_SHA" \
  --receipt "live release endpoint and served bundle matched" --as codex@driver --json

kb deploy current --json
kb deploy list --status failed --json
kb deploy show "$DEPLOYMENT_ID" --json
```

Success is deliberately strict: it requires a live verification receipt and a
served commit exactly equal to the requested full 40-character commit. A retry
is a new attempt linked with `--retry-of`; an idempotent caller supplies
`--operation-id`. See ADR-030. The cross-board live matrix is at
`https://kb.geoy.ws/deployments`.

### Recovering an artifact whose build commit is unknown

Some releases are a retained image and nothing else: no build SHA anyone can
trust, and inventing one is the failure the ledger exists to prevent. Such an
attempt names its identity explicitly instead.

```bash
kb deploy start --repo geoyws/legacy-stack \
  --artifact "api=docker-image-id:sha256:$API_IMAGE_ID" \
  --artifact "web=oci-manifest-digest:sha256:$WEB_MANIFEST_DIGEST" \
  --build-commit unknown --deployer-checkout "$FULL_SHA" \
  --tier @_p --environment production --host hax --url https://legacy.geoy.ws \
  --as codex@driver --json

kb deploy finish "$DEPLOYMENT_ID" --token "$CAPABILITY_TOKEN" \
  --result succeeded --phase verification \
  --observed "api=docker-image-id:sha256:$API_IMAGE_ID" \
  --observed "web=oci-manifest-digest:sha256:$WEB_MANIFEST_DIGEST" \
  --receipt "pulled both images on the tier and read their identities" \
  --as codex@driver --json
```

One mode per attempt, named by the caller: `--commit` is refused in artifact
mode and `--artifact` in Git mode. `--build-commit unknown` is required rather
than defaulted, so the absence is stated; a full 40-character SHA there is
refused and sends you to `--commit`. A `KIND` is `docker-image-id` (a Docker
config/image ID) or `oci-manifest-digest` (a registry manifest digest), and the
two are never compared to each other. A succeeded finish needs one `--observed`
per expected role, exact per role and per kind; a missing role, an extra role, a
kind mismatch or a value mismatch is refused naming the role and both values,
and `--served-commit` is refused outright, so a digest-only success cannot be
recorded as a verified Git commit.

`--deployer-checkout` is the checkout the deployer ran from. It is stored in its
own field and is never presented as the build commit. Every projection —
`deploy show/list/current --json`, the MCP tools, `kanban schema` and the
Deployments page — carries `identityMode`, `buildCommit` (`unknown` here),
`buildCommitLabel`, `deployerCheckout` and `artifacts[{role, kind, expected,
observed}]`, and renders `build commit unknown - recovered by artifact identity`
in words rather than a blank where a SHA would be. See ADR-043.

## Off-site backup

```bash
./scripts/backup.sh                 # snapshot, encrypt, upload, verify, prune
./scripts/backup.sh --rehearse      # restore the newest remote copy and doctor it
./scripts/backup.sh --verify-only kanban-<stamp>.tar.gz.age
```

`kanban-backup.timer` runs it nightly at 02:50 and keeps 14 copies, age-encrypted,
on the Hetzner Storage Box. `kanban-restore-rehearse.timer` runs a real restore
monthly into a scratch data root and makes the restored copy answer `doctor`,
because a backup nobody has restored is a backup nobody knows works. Neither
touches the live data root.

The verify step is not optional: every run re-downloads what it just uploaded,
decrypts it, and asserts the registry and every board came back as valid SQLite.
Each local snapshot also carries `manifest.json`, with the exact database set,
byte sizes, SHA-256 digests, schema versions, and audit heads. Retain that file
independently when an adversarial rollback matters; it becomes an external
anchor that can be checked with `kb audit verify --against manifest.json`.
An unverified backup is a hope, not a backup.

The decryption key is the **only** copy — `keys/kanban-backup-age.key` in the
git-crypt'd dotfiles, which is where it must live, because it has to survive the
loss of the machine the backups are taken from.

## The web view

```bash
kanban serve --port 14200                     # loopback only; no --bind flag exists
kanban serve --socket /run/kanban/kanban.sock # proxy-only socket, mode 0660
```

Exactly one listener: both flags together, or neither, is a usage error and
exits 64 rather than 1, so a supervisor can tell a bad unit file from a
listener worth restarting. A `--socket` path is created mode `0660` owned by
the serving uid and its primary group — add nginx's `www-data` to that group
and nothing else on the box can connect. The path must be absolute, and one
already holding anything other than a stale socket of this uid's, or whose
parent directory is a symlink, is refused rather than replaced.

Ten server-rendered views over every registered board: open attention items
across all of them by priority then age, the dashboard projection, draft plans
with the work each holds back, the verified deployment matrix and attempt
detail, `/lanes` lane sitreps, `/subscriptions` delivery state, cross-board
cited search, one board's rows, and one task in full.
Priority badges use P0/P1/P2 everywhere a queued row appears.
Every read goes through the same `Store` methods the CLI calls, so there is no
second implementation to keep in step.

The Plans page can open an existing draft epic as `geoyws`, moving it to `todo` and
releasing its child work for claims. It requires a same-origin POST and refuses
any row that is not currently a draft epic.

`/subscriptions` says in words what each subscription watches, where it
delivers, and where it has actually got to: the start anchor, the highest acked
seq, the distance to that board's event head, and the pending, retrying and
dead-lettered counts, with dead-lettered flagged as needing a person. A
configured `secretRef` renders as the fact that a secret is configured, never
as its value. Pause and resume are same-origin POSTs through the same audited
Store operations as `kb subscription pause`/`resume`, and repeating one is a
no-op that still lands on the page. Whether paused rows are listed is
`?show=all` in the URL and nothing is stored: no cursor, no preference row.
Cursor presentation means showing a cursor's meaning — a cursor held in a
browser would be a claim the server must trust, and a stale one silently skips
rows (`docs/ui-pubsub-consumption-seams.md`).

The **Needs you** page is the deliberately narrow exception to the read-only
surface: every open item renders as a decision card and one click settles it as
`geoyws`. Same-origin checks, strict bounded form decoding and the Store's
duplicate-resolution refusal guard the write.

A card reads top to bottom in the order it is decided in
([ADR-042](docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md)
§5): the question as the heading, the context, then the recommended choice
first — marked `recommended` and the first interactive element in the card —
with each choice's consequence beneath its button, then the card's one reply
field, then the free-text answer with its four-value outcome picker, then the
body folded under `show the full item`, then the meta line. The reply field
sits with the choices, above the rule that starts the free-text answer,
because it serves both: whatever is written in it rides with whichever choice
is clicked, and the free-text answer is that same field plus a verdict.
`1`–`4` answer the card that has focus in the order it lists them, so `1` is
always the recommendation, and `c` reaches the reply field; both are inert
while a reply is being typed. A row whose raiser authored no card is the same
card with the `approve`/`reject` default pair, its first body line as the
question and nothing marked recommended.

One click posts `decision=<key>`, carrying `reply=<text>` as that decision's
note when the field has words in it; the free-text answer posts
`decision=custom&outcome=<verdict>&reply=<text>` and is refused without both
by the page and by the composer that writes the trail. Nothing navigates: the
card is replaced in place by a one-line receipt naming the choice — and saying
the reply is recorded when one rode with it — plus the
`kanban attention reopen <id>` that undoes it, the open count drops, and the
receipt survives the live refresh — a reload on a 133-item list
would throw the reader back to the top of it. A key the row no longer carries
is refused by name rather than mapped onto whatever now sits in that position,
so a card left open in a tab is safe to click. Every other route remains
read-only: the browser can resolve an attention item, open a draft plan, and
pause or resume a subscription, and nothing else, enforced by the source
mutator allowlist and byte-for-byte process-boundary tests.

`/live` is a WebSocket notification channel. It sends only revision notices and
heartbeats; the browser fetches the canonical server-rendered page after a board
changes. It never sends board content, cookies, credentials or lease tokens, and
it defers a refresh while a reply is being typed. The compatibility socket
stays on `/live`; the canonical long-running subscription is `kb watch`, which
reads the append-only ledgers directly and leaves `kb events` as the newest-
first snapshot view. `kb watch` uses the additive protocol-v1 envelope and the
same fail-closed cursor rules described in ADR-031.

Durable delivery intent is managed separately from the live process:

```bash
kb subscription add --project NAME --id sub-codex-queue \
  --subject task:t-12345678 --kind checkpoint_added --tag orchestration \
  --consumer codex.queue --action enqueue-turn \
  --timeout-ms 30000 --max-retries 3 --rate-per-minute 60 \
  --max-concurrency 1 --secret-ref codex_queue_token --as geoyws --json
kb subscription list --project NAME --json
kb subscription pause sub-codex-queue --project NAME --as geoyws --json
kb subscription resume sub-codex-queue --project NAME --as geoyws --json
```

Subscriptions are board-local declarative records. The selected board is the
board predicate; rows store no board path or root. Predicates, consumer/action
names and numeric policy are normalized and validated fail-closed. The optional
`secretRef` is a strict opaque lookup identifier, never a credential. Lifecycle
mutations are audited on the board ledger, while `subscription list` and
`subscription show` remain read-only request-response operations in the
generated command schema.

The first queue bridge is the Codex consumer `codex.queue` with action
`enqueue-turn`. The subscription row still stores only the normalized
predicates, the named consumer/action, and bounded policy. It does not store
an executable path, shell text, or arbitrary args. Host-local
`dispatchers.json` binds that consumer/action to the checked-in
`kanban-codex-queue-adapter`, the installed Codex executable, the exact
thread/session target, the required installed version, and the fixed host-local
Codex state directory argument `--codex-home /root/.codex` with matching
`CODEX_HOME=/root/.codex`. Every invocation must
direct-exec the installed Codex binary to verify the exact version and the
`codex queue --help` surface, and it must fail closed on drift. The adapter
passes only that fixed `CODEX_HOME` to child Codex processes. On HAX, direct
ingress is `/root/.local/bin/codex queue --thread UUID_OR_EXACT_SESSION_NAME
--message TEXT` when `codex-cli 0.150.1` is installed. This contract is not
itself the live-smoke receipt; the separately named HAX live smoke receipt is
the distinct runtime check for installed Codex support.

The second experimental and opt-in bridge uses consumer `codex.app-server`,
action `start-readonly-turn`, and capability `start`, implemented by
`kanban-codex-app-server-adapter`. The host allow-list binding
is installed, but this rollout enables no active declarative subscription.
When the dispatcher invokes it, the adapter still accepts a structured
`AdapterRequest` and returns `AdapterResponse`; that is the normal dispatcher
path, not a general subscription bypass. Its operator synopsis, pins, and
fail-closed rules live in `docs/PRD.md`; the architecture and acceptance
matrix live in
`docs/adr/ADR-031-ledger-first-pubsub-uses-the-append-only-event-ledger.md`.
The host pins Codex CLI `0.150.1`, the `ClientRequest` hash
`efcd14b3433960c5e64a294e0071d48150429a603a5a18df536c84b76a902317`, and the
combined v2 schema hash
`8cdccfc35582696d7141e7f916e0d5a664ab5b5e90b732f104284d2507f369f8`; each
invocation clears child environment except `CODEX_HOME`, probes version/help,
and emits only `AdapterResponse` on success.

**HAX live smoke receipt, 2026-09-05.** Installed-Codex support is now
established for this bridge, and separately from the compiled fake-Codex
contract test. Against `@@hax`'s installed `codex-cli 0.150.1`, the host's own
`dispatchers.json` binding was invoked with one structured `AdapterRequest`
(subscription `smoke-t-560b778b`, a 64-hex event ID) and exited `0` in ~6s
having written only an `AdapterResponse` to stdout and nothing to stderr. The
turn mutated nothing: the private cwd
`/root/.local/share/kanban/codex-app-server-cwd` was byte-for-byte the same
directory listing before and after, the host's tmux panes and their foreground
commands were unchanged, and the adapter's identity-pinned schema temp dir was
removed — a `find /tmp -newermt '-3 minutes'` sweep for its own scratch
returned nothing. A malformed request is still refused rather than coerced:
a non-hex event ID exits `1` with `adapter delivery event ID must be
lowercase 64-hex` and never reaches Codex. The interface stays **experimental
and opt-in** after this receipt, and no active declarative subscription ships.

The third opt-in bridge uses consumer `claude.print`, action
`start-readonly-turn`, and capability `start`, implemented by
`kanban-claude-print-adapter`. No active declarative subscription ships.
Host-local configuration pins the canonical Claude executable, private
`HOME`, private cwd, and required Claude Code version (the HAX stable version
is `2.1.236`). Every invocation revalidates those identities and ancestor
chains, probes the exact version and required help surface, clears the child
environment to exactly `HOME` and `PATH=/usr/bin:/bin`, and starts a fresh
`--safe-mode --print` worker with no tools, MCP, persistence, or permission
prompts. It never resumes a foreground session. Only bounded subscription and
event IDs enter the exact acknowledgement prompt. Success requires strict
object-or-array JSON whose final result is that exact acknowledgement, with no
tool evidence, API error, stderr, trailing JSON, overflow, or mismatch; adapter
stdout is then only `AdapterResponse`. Compiled adapter-contract coverage uses
a dependency-free fake Claude. Installed-Claude support requires a separately
named live smoke and is not established by that compiled test.
The installed-Claude live smoke is currently blocked by revoked OAuth attention
`a-347ff24c`; the adapter does not work around authentication.

The declaration-only phase historically stopped there. The shipped delivery
worker is now a separate compiled process with one explicit board selector:

```bash
kanban-dispatcher --project NAME [--consumer consumer.name] [--once] [--json]
kanban-dispatcher --workspace /registered/root [--once] [--json]
kanban-dispatcher --db /exact/board.db [--once] [--json]
```

It resolves consumer/action capabilities from the operator-owned protocol-v1
`$KANBAN_DATA_DIR/dispatchers.json`, never from a subscription row. The file
maps strict consumer and action names to an allow-listed capability, one
absolute executable and fixed arguments; an optional secret reference maps a
named source environment variable to one safe adapter environment variable.
The private data root and config must not be group- or other-accessible, and an
adapter executable must be a regular, executable, non-symlink file that is not
group- or other-writable. Do not put a credential value in the config, board,
command line, documentation, or adapter arguments.

Startup validates configuration before the first materialization or claim.
Each scheduler step then materializes and recovers durable work, resolves its
candidate before lock contention, serializes per consumer, reloads and
revalidates configuration under that lock, and claims with a lease equal to the
adapter timeout plus 30 seconds. Adapter execution is outside the SQLite
transaction. Success and failure close only the exact lease token;
deterministic retry eventually dead-letters exhausted work. Pause and resume
are rechecked at claim time, SIGINT/SIGTERM stop polling and cancel a running
adapter cleanly, and a crash after adapter success but before acknowledgement
is recovered after lease expiry. Delivery is therefore at-least-once, with
immutable identity `(subscriptionID,eventID)` as the idempotency key rather
than an exactly-once promise.

Kanban implements no authentication: it binds `127.0.0.1`, or a `0660` Unix
socket, and trusts the edge.
The persisted target edge is nginx `auth_request` backed by the shared Google
SSO at `https://kb.geoy.ws`; only `geoyws@gmail.com` is allowed, and the
`.geoy.ws` session cookie is shared with Paste, Snip and Docs. When the web
surface runs in opt-in actor-header mode, nginx must strip any client-supplied
copy and set `X-Auth-Request-Email` from a successful `auth_request`; Kanban
uses that normalized value as the audit actor and still requires same-origin.
That `--actor-header` mode may be enabled only while nothing but the edge can
reach the server: loopback, or a socket, which is strictly more local than
loopback rather than a widening of it. The `kanban-serve.service` keeps the
process up. There is
deliberately no
`--bind` flag — any value other than loopback publishes an unauthenticated
surface.
Reasoning: `docs/adr/ADR-016-*`.

## Short names

The crate installs ten executables, and the HIG release package ships all ten:
`kanban` and `kb` are two names for the same operator CLI, alongside
`kanban-dispatcher`, `kanban-codex-queue-adapter`,
`kanban-codex-app-server-adapter`, `kanban-claude-print-adapter`,
`kanban-opencode-adapter`, `kanban-kimi-acp-adapter`,
`kanban-cursor-worker-adapter`, and `kanban-zcode-notify-adapter`. `kb` is a
real binary rather than a shell alias because agents invoke it from
non-interactive cages that never source a shell profile.

Installing a release flips `current`, relinks all ten public binaries, then
restarts `kanban-serve` and proves the process that came back is the release
it just installed: the unit reports `active` with a `MainPID`, that pid's
`/proc/<MainPID>/exe` IS the `kanban` executable retained in the new
`releases/<id>` — not merely a path inside it, so `kb` and the kernel's
`<path> (deleted)` are refused — and the listener its own `ExecStart` names,
`--port N` or `--socket PATH`, answers 200. Every request is bounded by what
is left of the deadline, and the pid and its exe are read once more after the
200, because a 200 proves only that something answered. The measurement lands
in the install receipt as
`serve: {restarted, mainPid, exe, exeSource, listener, http}`, where
`exeSource` is `/proc/<pid>/exe` on a host that has one and names the test
override where it does not. A proof that fails is not a warning: the previous
`current` and links are restored, the unit is restarted and the previous
release PROVED back into service before the candidate's directory is removed
— a first install with nothing to fall back to stops the unit first — and a
recovery that cannot be proved reports both failures and keeps the candidate
on disk to recover from. A host with no such unit prints
`serve restart skipped: <reason>` and records `serve: {skipped: <reason>}`
instead of claiming a restart that never happened; a host whose service
manager cannot be asked is a failed install, because a skip is a claim about
the unit and an unreachable manager supports none.

`hig-release.sh rollback` owes the same proof and now performs it: it restarts
the unit and proves the release it rolled back to is the one serving, and its
summary carries the same `serve` measurement.

What none of this can undo is a schema migration. Rolling the CODE back does
not roll a store back: a release that has already opened a board migrates it,
and the older binary the recovery restores may then be unable to read it. In
that case the recovery is simply unproved — the failure is reported next to
the original one, the candidate release is kept on disk, and nothing is
repointed at it automatically. No database is rolled back or restored by the
release path, ever; a store that has moved forward is an operator decision
with the backups (`kb backup`, `kb restore`), not something an installer may
make on its own.

Commands and subcommands have short forms:

| Scope | Aliases |
| --- | --- |
| command | `t`=task · `s`=story · `h`=handoff · `w`/`ws`=workspace · `cp`=checkpoint · `hb`=heartbeat · `ctx`=context · `dash`=dashboard · `rel`=release · `n`=note · `sr`=sitrep · `v`=version |
| `task` | `ls`=list · `mv`=move · `rm`=remove · `new`=add · `up`=update · `meta`=metadata · `cat`=show |
| `story` | `adv`=advance |
| `handoff` | `ls`=list · `new`=create · `acc`=accept |
| `workspace` | `ls`=list · `att`=attach |
| `sitrep` | `ls`=list · `new`=post |
| `subscription` | `ls`=list · `new`=add · `cat`=show |

```bash
kb t ls --status todo
kb t mv t-resume done --as deepseek
```

Aliases resolve by exact match against the table above. Unlisted short forms
stay unknown commands, and flags are never abbreviated — `--proj` is an error
that suggests `--project`, not a synonym for it. Prefix inference would mean a
command or flag added later silently retargets callers that already work
([ADR-008](docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)).

Sub-aliases apply only where the second word is a subcommand, so a task whose
id happens to be `rm` is still addressable.

## Staleness, audit and recovery

`stale_minutes` is a per-task budget for how long work may sit without a
signal. `kb stale` lists tasks that have overrun theirs, measured from the
claim heartbeat when there is one and from `updated_at` otherwise, and
`kb dashboard` carries the count per project plus `highestPriority` and
`highestPriorityLevel`, and orders projects by that priority then the oldest
row at the level. Projects with no queued work sink to the end.

`kb dashboard` also carries `gatedTasks` per project: the unarchived,
unfinished task rows waiting on a completion gate, whether declared on the row
or inherited from a plan above it. It sits beside `taskCounts` rather than
inside it, because `taskCounts` is the raw status tally and a gated `todo` row
is still `todo` — subtracting or renaming it would leave the board's own
arithmetic not adding up.

```bash
kb stale --json           # [{ id, staleMinutes, idleMinutes, overdueMinutes, lastSignal }]
kb events --kind lease_seized
kb events --task <id> --after START_MS --before END_MS --json
kb events --task <id> --all --after START_MS --before END_MS --json
kb watch --project NAME --json
```

`events`/`ev` read board history in a half-open millisecond window:
`[after,before)`. SQL filters happen before `--limit`, `--all` includes
archived rows, and registry or rule scopes reject `--after`/`--before`.

Snapshots are restorable, not just writable:

```bash
kb backup --keep 7                 # snapshot, then keep the newest 7
kb restore --from <SNAPSHOT> --force
kb audit verify --against <SNAPSHOT>/manifest.json --json
```

`restore` verifies the manifested file set, byte digests and audit chains
before it touches live state, refuses without `--force`, and writes a
`pre-restore-<stamp>` rescue snapshot of
what it replaced, including its own manifest. It creates no rescue directory and
touches no board file until it has classified every path it is about to write.
What it overwrites is `<root>/boards/<file name>` for each board in the
snapshot — decided by the snapshot and the filesystem, not by the registry, so
a file the registry no longer lists is still rescued before being replaced.

The test at each of those paths is whether the file can be *copied*, not
whether it is a healthy board, because a rescue copy needs the bytes and
nothing more. A board that opens is copied with SQLite's online backup, which
is WAL-correct. Anything else readable — corrupt pages, a damaged header, a
file that was never a board — is copied verbatim into `unparsed/` in the rescue
snapshot and listed under `unparsedFiles` in its manifest and `rescuedUnparsed`
in the receipt, so it is replaced but never silently. That matters because a
corrupt board is the disaster `restore` exists for: refusing to run on one
would block the only command that recovers it. `restore` refuses only when a
file's bytes cannot be read at all, which is the one case where no copy is
possible — and then the message says the data is very likely intact, because
nothing opened the file to find otherwise. Fix the permissions and run it
again, or move the file aside. The receipt names the restored
and rescue journal heads, so a deliberate rollback remains visible. It also takes
the data root exclusively and refuses outright while any other kanban process
holds it — you do not have to remember to stop them. `--keep` prunes only the
managed backups directory and only the stamped snapshots Kanban itself wrote —
never a directory reached via `--output`, and never a rescue snapshot.

## Failing closed

Kanban is driven by agents that cannot notice a mistake: a turn issues a
command, reads the exit status, and moves on. So an operation that cannot be
interpreted unambiguously is refused rather than guessed
([ADR-008](docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)).

- **Unknown flags are errors.** Every flag is declared per command, and an
  unrecognized one names the nearest match. A mistyped `--projct alpha` used to
  fall through to directory resolution and write to whichever board contained
  the working directory.
- **Extra arguments are errors, not dropped.** Every command declares how many
  positionals it takes. `kanban task add Fix the parser` used to record the
  title `Fix` and report success; `kanban note t-1 the build is red --as ci`
  recorded `the`. The error shows the prefix it accepted, so where the title
  stopped is obvious.
- **Two requests at once are refused.** `claim t-5 --next` named a task *and*
  asked for whichever comes first; it used to drop the id and hand back a
  different task. A single-valued flag given twice is an error rather than
  last-wins — `--project alpha --project beta` wrote to beta. Flags whose value
  is a list (`--depends-on`, `--blocker`, `--validation`) still repeat.
- **A live lease is not overridden silently.** `task move` and `task remove`
  refuse against a claimed task, naming the holder and its expiry. `--force`
  seizes the lease and writes a `lease_seized` event; a forced removal also
  records how many notes and checkpoints it discarded.
- **`init` will not shadow an enclosing project.** Running it inside a
  registered tree points at `kanban workspace attach --to ROOT`, which is
  almost always what was meant. `--force` creates a genuinely separate nested
  board.
- **A dead lease is retired before anything reads it.** Expiry used to happen
  only when a claim was attempted, so a vanished agent left its task reading
  `in_progress` on every read path while `claim --next` gave the same task
  away. Every board command now sweeps first, and each expiry is recorded as a
  `claim_expired` event.
- **Priority is P0, P1 or P2 on every actionable queue.** P0 interrupts, P1 is
  committed to the current cycle, and P2 is routine and the default. Tasks,
  attention items and handoffs accept those symbols case-insensitively; the
  compatible numeric key remains `0` through `9` (`0`–`2` P0, `3`–`5`
  P1, `6`–`9` P2), so existing rows and controlled within-band ordering are
  preserved. JSON returns both `priority` and `priorityLevel`. See
  [ADR-025](docs/adr/ADR-025-priority-levels-are-p0-p1-p2-across-queues.md).
- **`context` declares what it dropped.** `truncated` is computed by
  over-fetching past each cap, so a resuming agent is never told it holds the
  complete record when it does not. The rendering says so too: any output
  shorter than the complete one carries `[older history omitted]` or
  `[context compacted: …]`, and the marker is reserved out of `--max-chars`
  rather than appended to it — at the smallest budgets, where compaction
  matters most, the marker used to be the first thing cut. `--max-chars` with
  `--json` is an error, because it bounds the rendered text and never bounded
  the packet.
- **A capped listing refuses a default it would exceed.** `events` (50),
  `sitrep list` (20), `search` (10), `attention list`, `deploy list`,
  `handoff list` and `claim --candidates` (100), and `task show`'s notes (100),
  checkpoints (20) and handoffs (100) each fetch one row past their default;
  if it is there and no `--limit` was given, the listing exits non-zero and
  names `--limit N` rather than handing back a page that reads as the whole.
  `attention list --json` once returned exactly 100 rows that a session
  report called "100 open attention" when there were 157. Exactly the default
  with nothing past it is complete and answered as such — the extra row is
  looked for, never inferred from the count — and an explicit `--limit` is
  honoured as stated, with no marker in the payload
  ([ADR-037](docs/adr/ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md)).
- **`--limit` is bounded at 1000000, and that is a typo guard.** Every surface
  that takes the flag — `events`, `search`, `watch`, `attention list`,
  `sitrep list`, `deploy list`, `handoff list`, `claim --candidates`,
  `task show` — refuses a value above the ceiling with one wording, and the
  ceiling is past every board this tool holds, so it is effectively unbounded
  for real use and catches a slipped keystroke rather than turning it into a
  query. Defaults are unchanged. `events` alone names an explicit `--limit`
  that cut, on stderr, in one line; stdout stays exactly the rows asked for and
  the exit status stays zero, because a page of history that stops at the limit
  reads as the whole history (ADR-037 addendum, 2026-09-08).
- **A restore cannot race live work.** `restore` is the one operation that goes
  around SQLite, renaming whole database files into place. It now takes the
  data root exclusively and refuses while anything else holds it; board
  commands take it shared, so they never block each other, and a command that
  meets a restore in progress waits five seconds and then says so rather than
  reading a half-replaced root. A board named by path outside the data root
  (`--db /tmp/scratch.db`) is untouched by any of this.
- **A `--json` refusal is JSON on stdout.** Every refusal exits non-zero and
  names its fix on stderr; with `--json` the same message also reaches stdout
  as `{"error": "…"}`, and nothing else does. `claim --candidates --json`
  without `--as` used to leave stdout empty, so a consumer piping it into a
  parser read a valid-looking empty candidate list while P0 rows sat in todo.
  The one shape that differs is a command whose report *is* the answer:
  `doctor` and `audit verify` print their JSON report and then exit non-zero
  to say it was unhealthy, so no error object follows it and the report stays
  parseable.

Overrides are reviewable, because forcing one writes durable history:

```bash
kb events --kind lease_seized        # who overrode whose lease, and when
kb events --task t-resume --limit 20
kb events --registry --limit 20      # rules and workspace lifecycle
```

Every command still prints JSON whether or not `--json` is passed.

## Current scope

Version 0.3 is the compiled Rust private multi-project CLI and continuity slice.
It includes worktree aliases, aggregate dashboard reads, lane-aware claims,
structured handoffs, integrity checks, snapshots, and legacy atmux task import.
Planned adapters:

1. opencode-plugin-orch long-horizon workflow;
2. atmux feature parity, state importer, compatibility adapter, and verified
   removal of atmux's duplicate Kanban implementation;
3. MCP surface for other compatible harnesses;
4. optional board UI and cross-host synchronization.

Integration handoffs: [orch](docs/integrating-orch.md) and
[atmux](docs/integrating-atmux.md).

See the [product requirements](docs/PRD.md),
[ADR-001](docs/adr/ADR-001-durable-agent-work-ledger.md),
[ADR-003](docs/adr/ADR-003-private-multi-project-personal-work-system.md),
[ADR-004](docs/adr/ADR-004-token-pressure-handoffs-through-kanban.md), and
[ADR-008](docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md).

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

`cargo test` runs unit tests for the pure logic (flag validation, the
nearest-match hint, the alias tables, context trimming) alongside the E2E
suite. Several are drift guards rather than behaviour tests: they assert every
command declares its flags without duplicates, every boolean flag is accepted
somewhere, and no alias shadows a real command — so extending the CLI without
extending its tables fails the gate.

The program lives in the crate's library; `kanban` and `kb` are thin binary
shims over it, so the crate is compiled once rather than twice. The E2E suite
invokes `CARGO_BIN_EXE_kanban` and `CARGO_BIN_EXE_kb` as separate
operating-system processes. It covers persistence and restart, concurrent claims, worktree
aliases, token-pressure handoffs, story gates, imports, backup/reopen, bounded
context, TODO projection, the released SQLite v3 format, and one real-browser
approval flow against the compiled server.

The database migration ladder is append-only. Never edit a released migration;
add the next rung.
