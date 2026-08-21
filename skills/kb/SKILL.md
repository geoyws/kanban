---
name: kb
description: The kanban work-ledger CLI — tasks, claims and leases, checkpoints, handoffs (task and session), and attention items that need George. Use whenever recording, reading or handing over work state, raising something only George can settle, or resuming a lane from the board rather than a directory.
argument-hint: "[subcommand …]"
---

# /kb — the kanban work ledger

`kb` and `kanban` are the same binary, installed under both names. It is a
durable, per-project SQLite ledger for agent work: what there is to do, who
holds it, what happened, what was handed over, and what needs George.

**Two rules shape everything below.** The ledger never states something that
isn't true, and it never does something other than what the caller asked. Every
refusal you meet here names the fix — pass the message on rather than
paraphrasing it.

Output is **always JSON**, with or without `--json`.

## Aliases

Aliases resolve by **exact match**. Prefix inference is refused in both
directions — `task li` is not `task list`, and `--proj` is not `--project`. A
near-miss flag is *suggested* in the error, never accepted.

| Short | Full |
|---|---|
| `t` | `task` |
| `s` | `story` |
| `h` | `handoff` |
| `att`, `attn` | `attention` |
| `w`, `ws` | `workspace` |
| `cp` | `checkpoint` |
| `hb` | `heartbeat` |
| `rel` | `release` |
| `ctx` | `context` |
| `ev` | `events` |
| `dash` | `dashboard` |
| `n` | `note` |
| `v` | `version` |

Scoped to their group:

| Group | Short forms |
|---|---|
| `task` | `ls`=list `mv`=move `rm`=remove `new`=add `up`=update `meta`=metadata `cat`=show |
| `story` | `adv`=advance |
| `handoff` | `ls`=list `new`=create `acc`=accept |
| `workspace` | `ls`=list `att`=attach |

⚠ `att` means **attach** inside `workspace` and **attention** at the top level.
Both are exact-match and scoped, so they never collide — but read
`kb ws att` as attach and `kb att` as attention.

## Addressing a board

Each project is one board. Give **exactly one** selector; two that disagree are
refused rather than ranked, because only one is what you meant and nothing in
the receipt would say which was used.

```bash
--project NAME      # a registered project, from any directory
--workspace PATH    # the project containing PATH
--db PATH           # a board file directly
```

`KANBAN_PROJECT` and `KANBAN_DB` are defaults a flag may override — a default is
not a second request. With none of them, the board is the one containing the
working directory.

```bash
kb ws ls --json                 # every registered project and its board path
kb init --name NAME             # register the current directory
```

## Attention — anything that needs George

**Raise it the moment you find it, every time.** A reply, a report and a commit
message are channels that scroll away: an item raised at 03:00 and never acted
on leaves no trace it was ever raised, so the same question gets asked again
three sessions later — or worse, quietly answered by an agent that had no
business deciding it.

```bash
kb att raise "<verdict-first, ≤2 sentences, with the concrete next action>" \
  --as "<agent>@<lane>" --kind blocking --task <ID if it is about one> --json

kb att list --status open --json          # what is waiting on George
kb att list --status resolved --json      # the historical trail
kb att resolve <id> --as geo --note "…"   # George settles it
```

`--kind` is a closed set:

| kind | use for |
|---|---|
| `blocking` | work cannot resume until he calls it |
| `decision` | a design or scope call that is his |
| `approval` | staging push, destructive op, scope expansion, spend |
| `review` | a deployed tier waiting on his eyes — put the URL in the text |
| `risk` | failed gate, flaky tier, expired credential, half-written file |

There is deliberately no `info`: something that needs nobody is a note, and
`kb n` already holds those.

**Raise, do not resolve.** Agents raise and read; only George resolves. The one
exception is an item this same session raised and has since made moot — retire
that with `--note` saying why, so the record shows it was withdrawn rather than
answered. Check `kb att list --status open` before raising and add to an
existing item rather than duplicating one already waiting.

Items are **resolved, never deleted**, and resolving twice is refused: that
would overwrite who settled it and when, which is the part worth keeping. Open
items list **oldest first** — an unanswered question does not get less urgent by
being ignored.

Still surface the item in your reply as well. The board makes it survive; the
reply makes George see it now. In one and not the other is a bug.

## Handoffs — task and session

A **task handoff** passes a claimed task to whoever comes next. It needs the
lease, writes a checkpoint, releases the lease, and returns the task to the
queue:

```bash
kb h new <task-id> --lease "$TOKEN" --as "$AGENT" \
  --summary "…" --intent "…" --next-action "…" --reason token_pressure --json
```

A **session handoff** is about the work as a whole — no task, no lease. This is
what a lane hands its successor:

```bash
kb h new --as "claude@driver-2" --to "driver-2" --reason session_end \
  --summary "…" --intent "…" --next-action "…" \
  --branch "$(git branch --show-current)" --repo "$(git rev-parse --show-toplevel)" --json
```

The task id and the lease travel together: each half alone is refused, because a
lease exists only over a task and a task cannot be handed over without one.

**Find one by lane, not by directory** — the point of the session form. A
worktree gets recreated, a driver renumbered, a repo cloned to another box; a
brief keyed to a path is then unreachable. The successor knows its project and
its lane, so that is the key:

```bash
kb h ls --project px-crm --status pending --to driver-2 --json
kb h acc <id> --as driver-2 --json      # accept once absorbed; no lease for a session handoff
```

`--repo` and `--branch` ride inside the record, so the successor `cd`s from the
record rather than the path ever having been the lookup key.

Handoffs are **history**: removing a task drops the link and keeps the account.

## Working a task

```bash
kb t new "Title" --priority 3 --lane fe --json     # 0 most urgent … 9 least, 3 default
kb t new "Half-formed idea" --status draft --json  # not ready for action yet
kb t ls --status todo --json
kb claim --next --as "$AGENT" --json               # or: kb claim <id> --as "$AGENT"
kb hb <id> --lease "$TOKEN" --lease-minutes 30     # renew
kb cp <id> --lease "$TOKEN" --as "$AGENT" --state continue \
  --summary "…" --intent "…" --next-action "…" --json
kb rel <id> --lease "$TOKEN"
```

`--state done` or `blocked` on a checkpoint **releases the lease in the same
transaction** that records it — there is no window where the work reads finished
but the lease is still held.

## Plans

**A plan is an epic.** Its body is the plan, its children are the work it became,
and `draft` is a plan saved up but not ready to act on. There is no separate plan
object: the container already exists, and `--parent` answers "what did this plan
produce" better than any link would.

```bash
kb t new "Q4 migration" --type epic --status draft --body-file plan.md --json
kb t new "Phase 1" --type epic --parent e-q4 --status draft --json   # a sub-plan
kb t new "Enumerate consumers" --parent e-q4-p1 --json               # the work
kb t mv e-q4 todo --as geo                                           # ready to act on
```

`--body-file` reads the body from disk, because a plan is markdown measured in
kilobytes. Passing `--body` and `--body-file` together is refused — two answers
to one question.

**Revising a plan keeps the old one.** `task update --body-file` records the
previous body on the event trail, so the plan's history is
`kb ev --task <epic-id> --json` and needs nobody to have kept a copy:

```json
{ "kind": "task_updated",
  "payload": { "changed": ["body"], "previousBody": "# Q4 migration\n…" } }
```

An epic holds epics, stories and tasks; a story holds tasks; a task holds
nothing. So plans nest and work hangs off them, while a story inside a story is
still refused.

A plan is not an ADR. An ADR records a decision and why it was taken; a plan
records intended work. Keep ADRs in the repo.

**A `draft` is not work yet.** It is the state before `backlog`: a row still
being written, whose title, body or scope may still be wrong. `claim --next`
skips it however urgent its priority, and naming it explicitly is refused.
Promote it with `kb t mv <id> todo --as "$ACTOR"` when it is ready to be acted
on. Use it for anything you are still specifying — an agent reads every row on
the board as a specification, and an unfinished one gets decomposed and worked
as though it were settled.

**Only a task is claimable.** An epic and a story are containers whose status is
derived from what is beneath them; `claim --next` skips them, and naming one
explicitly is refused pointing at `story advance` or at the children.

**A story's status is projected from its gate.** `kb t mv <story> done` is
refused — use `kb s adv <id> --as "$ACTOR"`. `blocked` and `cancelled` stay
directly writable, since the gate cannot express either.

**The tree is enforced**: an epic contains stories, a story contains tasks, a
task contains nothing.

## Reading

```bash
kb ctx <id> --json              # the bounded cold-start packet for a resuming agent
kb dash --json                  # per-board counts, incl. openAttention + pendingHandoffs
kb ev --task <id> --json        # the durable audit trail
kb stale --json                 # work that overran its stale budget
kb doctor --json                # integrity, orphaned rows, future-stamped tasks
```

`ev` is machine-written and append-only — `task_created`, `task_moved`,
`lease_seized`, `handoff_created`, `attention_raised`, `attention_resolved` and
so on. It records **what happened**; `att` records **what needs George**. Use
`ev` to reconstruct history, `att` to find open questions.

`ctx` is bounded and says so: `truncated` is computed, never assumed, and the
marker survives the truncation it describes.

`--limit` must be zero or more. A negative one reads as *no limit* in SQL, so it
is refused rather than silently handing back everything you asked to bound.

## As an MCP server

```bash
kb mcp     # newline-delimited JSON-RPC over stdio
```

One tool per operation, generated from the command surface, so the tool list
cannot describe something the CLI does not have. Each call runs the real binary,
so validation and refusals are identical to the terminal. Every tool carries
`readOnlyHint`, true only when the operation writes nothing anywhere.

Updating is `install` over the binary — running servers pick it up without any
client reconnecting.

## Refusals worth knowing

These are deliberate. Do not work around them; they exist because each one was
once a silent wrong answer.

- Unknown flags and extra positionals are **errors**, never dropped. Quote
  anything containing spaces.
- A single-valued flag given twice is refused — last-wins is how the wrong board
  gets written.
- `--force` is required to override a live lease or nest a board inside a
  registered tree, and every override is recorded.
- A diagnostic never modifies what it diagnoses; a missing registered board is
  reported, never silently recreated.
- `restore` takes the data root exclusively and refuses while anything else
  holds it.

## Reference

`docs/adr/` in the kanban repo carries the reasoning. Most load-bearing:
ADR-008 (fail closed), ADR-010 (adapters generated from the surface),
ADR-011 (MCP server + in-place reload), ADR-012 (session handoffs and
attention).
