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

## Project rules

Short, non-secret constraints that must frame every task belong to the project
board's ordered rules document:

```bash
kb r new "Production runtime is Rust." --as geo
kb r new --body-file /tmp/non-secret-rule.md --as geo
kb r ls                         # active table of contents, oldest first
kb r cat r-12345678             # fetch one full body lazily
kb r up r-12345678 --body "Production runtime is compiled Rust." --as geo
kb rule retire r-12345678 --as geo
kb r ls --all --full            # include retired rules and full bodies

# One document inherited by every project; do not pass a board selector.
kb r new "Never store credentials in Kanban." --global --as geo
kb r ls --global
kb r cat g-12345678 --global
kb ev --global --rule g-12345678 # audited revision/retirement trail

# Target one or more boards, or every board except named boards.
kb r new "Kanban-only rule." --global --board kanban --as geo
kb r new "Everywhere except project-a." --global --except-board project-a --as geo

# Narrow either kind to tasks carrying at least one named subsystem tag.
kb r new "Queuer-specific rule." --tag queuer --as geo
kb r new "Aix rule across boards." --global --tag aix --as geo
kb r up r-12345678 --clear-tags --as geo
```

The first line is the headline. Every context packet, newly granted claim and
accepted handoff carries the complete active table of contents: global rules
first, then project rules. Each compact summary names its scope, id, headline,
byte size and whether more exists; a long body is fetched only with `kb r cat ID` (plus `--global` for
a `g-` id). Other commands do not repeat rules. The claim receipt stays flat,
while a stored claim remains the same shape it had before rules existed.

Every global rule has explicit `boardTags`. Omitted targeting defaults to
`ALL`; named includes render as `ONLY:<board>`, and exclusions render as
`ALL, EXCEPT:<board>`. `--board` and `--except-board` are repeatable and accept
exact registered board names. See
[ADR-020](docs/adr/ADR-020-global-rules-use-explicit-board-tags.md).

Task-tag selectors are serialized as `taskTags`. Several are an OR set, while
their result intersects the global rule's board scope. Project selectors must
exist in that board's tag master; a global selector must be registered on at
least one active board. Claims, task handoff acceptance and task context inject
an active tagged rule only when the task carries at least one selector. A
session handoff has no task context, so it receives only untagged rules.
See [ADR-024](docs/adr/ADR-024-rules-target-task-tags-after-board-scope.md).

Rules are retire-only and audited: updating records the prior body, retirement
removes a rule from active contexts without deleting it, and there is no `rm`
alias. They do not replace private memory. Long-form context, cross-machine
knowledge and anything secret remain in versioned/git-crypt'd dotfiles. Never
put credentials or secret values in the plaintext board database. See
[ADR-018](docs/adr/ADR-018-project-rules-frame-work-without-replacing-private-memory.md)
and [ADR-019](docs/adr/ADR-019-global-rules-frame-every-project-on-claim-and-resume.md).

Attention rows carry the same registered subsystem vocabulary as tasks:

```bash
kb att raise "Review the deployed queuer" --as codex@driver --kind review --tag queuer
kb att list --status open --tag queuer
kb att update a-12345678 --body "Corrected request." --as codex@driver
kb att update a-12345678 --tag queuer --tag infra --as codex@driver
kb att update a-12345678 --clear-tags --as codex@driver
kb att resolve a-12345678 --as geo --note "Approved after review."
kb att reopen a-12345678 --as geo --note "Resolved the wrong item."
```

Several tags describe several touched subsystems. An agent may correct the body
or tags only while the item remains open; the prior body and tags stay on the
event trail, and the update does not settle the request. Resolving it freezes
the row with the rest of the historical receipt. Unknown tags are refused
rather than producing an empty-looking filter result.

Resolution is deliberately asymmetric: `geo` may settle any item, while an
agent may settle only an item whose `raisedBy` is that exact actor, and every
resolution requires a non-empty note. If a resolution was mistaken, only
`geo` or the recorded resolver may reopen it. Reopening returns the item to the
open queue without clearing `resolvedAt`, `resolvedBy` or `resolution`; it adds
`reopenedAt`, `reopenedBy` and `reopenNote`, and the transition is audited.

## Working from anywhere

Boards are addressable from any directory, not only from inside the project
tree. Board selection runs most explicit first ([ADR-007](docs/adr/ADR-007-global-project-addressing.md)):

| Selector | Meaning |
| --- | --- |
| `--db PATH` / `KANBAN_DB` | a board file directly |
| `--project NAME` / `KANBAN_PROJECT` | a registered project by name, from anywhere |
| `--workspace PATH` | the project containing `PATH` |
| _(none)_ | the project containing the working directory |

```bash
kanban task list --project my-project          # from any directory
export KANBAN_PROJECT=my-project               # or once per shell or agent cage
kanban task add "ships from anywhere"
kanban workspace list                          # the registered names to choose from
```

Project names are not unique. If two projects share one, `--project` refuses and
names the candidate roots; use `--workspace PATH` to pick one.

Initialize a project once, then attach additional Git worktrees to the same
board:

```bash
kanban init --name my-project --workspace /path/to/main-worktree
cd /path/to/another-worktree
kanban workspace attach --to /path/to/main-worktree
kanban workspace detach --root /path/to/retired-worktree --as geo

If a registered tree is later moved and a symlink left where it was, every path
still works at the shell but none of them resolve to the board: registration
stores a canonical path, resolution canonicalises the caller's cwd, and the two
spellings no longer meet. `doctor` reports those roots and where they lead now;
`kanban workspace repoint` points them at the tree's new home, repairing the
project root and every lane beneath it together.
When an attached worktree is intentionally gone, `workspace detach` retires
that alias without deleting its registry history; `workspace list --all` shows
the detached row. Canonical project roots cannot be detached.
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
Check and back up all registered boards with:

```bash
kanban version
kanban doctor
kanban backup --json
```

`version` prints the package version plus the newest board and registry schema
versions the binary supports. This makes two builds with the same package
version but different migration ladders visibly different before either one
opens a shared board. `doctor --json` also reports the registry's actual and
supported schema versions and, for every present board, its actual and
supported schema versions.

## SQLite-native retrieval and RAG

Kanban indexes its authoritative rows inside each board database. Search covers
tasks, notes, checkpoints, handoffs, attention, sitreps, project rules, selected
audit events, and applicable global rules. Every hit names the source with a
stable `kanban://BOARD/KIND/ID` citation; it does not synthesize an answer.

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
so `search` remains read-only. Only `search-rebuild` persists the cache and
writes an audit event. `--limit` and `--max-chars` bound agent context, and
`--source`, `--status`, `--tag`, `--lane`, `--after`, `--before`, and `--all`
filter it. `doctor` reports source/document/FTS parity plus cache freshness.

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
a task's sitreps so a resuming agent gets them without going looking.

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
An unverified backup is a hope, not a backup.

The decryption key is the **only** copy — `keys/kanban-backup-age.key` in the
git-crypt'd dotfiles, which is where it must live, because it has to survive the
loss of the machine the backups are taken from.

## The web view

```bash
kanban serve --port 14200      # loopback only; no --bind flag exists
```

Six server-rendered pages over every registered board: open attention items
across all of them by priority then age, the dashboard projection, draft plans
with the work each holds back, cross-board cited search, one board's rows, and
one task in full. Priority badges use P0/P1/P2 everywhere a queued row appears.
Every read goes through the same `Store` methods the CLI calls, so there is no
second implementation to keep in step.

The Plans page can open an existing draft epic as `geo`, moving it to `todo` and
releasing its child work for claims. This is the only browser write besides an
attention reply: it requires a same-origin POST and refuses any row that is not
currently a draft epic.

The **Needs you** page is the deliberately narrow exception to the read-only
surface: reply inline to resolve an attention item as `geo`. Same-origin checks,
strict bounded form decoding and the Store's duplicate-resolution refusal guard
the write. Quick replies are available on a phone without removing free text.
Every other route remains read-only, enforced by the source mutator allowlist
and byte-for-byte process-boundary tests.

`/live` is a WebSocket notification channel. It sends only revision notices and
heartbeats; the browser fetches the canonical server-rendered page after a board
changes. It never sends board content, cookies, credentials or lease tokens, and
it defers a refresh while a reply is being typed.

Kanban implements no authentication: it binds `127.0.0.1` and trusts the edge.
The persisted target edge is nginx `auth_request` backed by the shared Google
SSO at `https://kb.geoy.ws`; only `geoyws@gmail.com` is allowed, and the
`.geoy.ws` session cookie is shared with Paste, Snip and Docs. The
`kanban-serve.service` keeps the process up. There is deliberately no `--bind`
flag — any value other than loopback publishes an unauthenticated surface.
Reasoning: `docs/adr/ADR-016-*`.

## Short names

The crate installs two binaries, `kanban` and `kb`, which are the same program.
`kb` is a real binary rather than a shell alias because agents invoke it from
non-interactive cages that never source a shell profile.

Commands and subcommands have short forms:

| Scope | Aliases |
| --- | --- |
| command | `t`=task · `s`=story · `h`=handoff · `w`/`ws`=workspace · `cp`=checkpoint · `hb`=heartbeat · `ctx`=context · `dash`=dashboard · `rel`=release · `n`=note · `sr`=sitrep · `v`=version |
| `task` | `ls`=list · `mv`=move · `rm`=remove · `new`=add · `up`=update · `meta`=metadata · `cat`=show |
| `story` | `adv`=advance |
| `handoff` | `ls`=list · `new`=create · `acc`=accept |
| `workspace` | `ls`=list · `att`=attach |
| `sitrep` | `ls`=list · `new`=post |

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

```bash
kb stale --json           # [{ id, staleMinutes, idleMinutes, overdueMinutes, lastSignal }]
kb events --kind lease_seized
```

Snapshots are restorable, not just writable:

```bash
kb backup --keep 7                 # snapshot, then keep the newest 7
kb restore --from <SNAPSHOT> --force
```

`restore` verifies every file in the snapshot before it touches live state,
refuses without `--force`, and writes a `pre-restore-<stamp>` rescue snapshot of
what it replaced, so a mistaken restore is recoverable in turn. It also takes
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
- **A restore cannot race live work.** `restore` is the one operation that goes
  around SQLite, renaming whole database files into place. It now takes the
  data root exclusively and refuses while anything else holds it; board
  commands take it shared, so they never block each other, and a command that
  meets a restore in progress waits five seconds and then says so rather than
  reading a half-replaced root. A board named by path outside the data root
  (`--db /tmp/scratch.db`) is untouched by any of this.

Overrides are reviewable, because forcing one writes durable history:

```bash
kb events --kind lease_seized        # who overrode whose lease, and when
kb events --task t-resume --limit 20
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
context, TODO projection, and the released SQLite v3 format.

The database migration ladder is append-only. Never edit a released migration;
add the next rung.
