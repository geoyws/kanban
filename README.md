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
kanban task add "Implement durable resume" --id t-resume --priority 1

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
kanban doctor
kanban backup --json
```

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
clears obsolete imported claims, preserves durable history, and reports
created versus updated counts. It is a pre-cutover operation, not a dual-write
mode.

Epics, stories, tasks, hierarchy, stable IDs, timestamps, routing fields,
notes, and unknown extension JSON are preserved. Dangling historical
relationships are retained as warning metadata rather than inserted as invalid
foreign-key edges.

## Short names

The crate installs two binaries, `kanban` and `kb`, which are the same program.
`kb` is a real binary rather than a shell alias because agents invoke it from
non-interactive cages that never source a shell profile.

Commands and subcommands have short forms:

| Scope | Aliases |
| --- | --- |
| command | `t`=task · `s`=story · `h`=handoff · `w`/`ws`=workspace · `cp`=checkpoint · `hb`=heartbeat · `ctx`=context · `dash`=dashboard · `rel`=release · `n`=note · `v`=version |
| `task` | `ls`=list · `mv`=move · `rm`=remove · `new`=add · `up`=update · `meta`=metadata · `cat`=show |
| `story` | `adv`=advance |
| `handoff` | `ls`=list · `new`=create · `acc`=accept |
| `workspace` | `ls`=list · `att`=attach |

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

## Failing closed

Kanban is driven by agents that cannot notice a mistake: a turn issues a
command, reads the exit status, and moves on. So an operation that cannot be
interpreted unambiguously is refused rather than guessed
([ADR-008](docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)).

- **Unknown flags are errors.** Every flag is declared per command, and an
  unrecognized one names the nearest match. A mistyped `--projct alpha` used to
  fall through to directory resolution and write to whichever board contained
  the working directory.
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
- **`context` declares what it dropped.** `truncated` is computed by
  over-fetching past each cap, so a resuming agent is never told it holds the
  complete record when it does not.

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
