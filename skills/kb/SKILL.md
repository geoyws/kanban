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
| `sr` | `sitrep` |
| `w`, `ws` | `workspace` |
| `cp` | `checkpoint` |
| `hb` | `heartbeat` |
| `rel` | `release` |
| `ctx` | `context` |
| `ev` | `events` |
| `dash` | `dashboard` |
| `n` | `note` |
| `r` | `rule` |
| `v` | `version` |

Scoped to their group:

| Group | Short forms |
|---|---|
| `task` | `ls`=list `mv`=move `rm`=remove `new`=add `up`=update `meta`=metadata `cat`=show |
| `story` | `adv`=advance |
| `handoff` | `ls`=list `new`=create `acc`=accept |
| `workspace` | `ls`=list `att`=attach |
| `tag` | `ls`=list `rm`=remove `new`=add |
| `rule` | `ls`=list `new`=add `up`=update `cat`=show |
| `sitrep` | `ls`=list `new`=post |

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

## Project rules — what frames every task

Put short, non-secret operating constraints that apply across this project in
the board's ordered rules document:

```bash
kb r new "Production runtime is Rust." --as "$AGENT" --json
kb r new --body-file /tmp/non-secret-rule.md --as "$AGENT" --json
kb r ls --json                         # active table of contents, oldest first
kb r cat r-12345678 --json             # one full body, fetched lazily
kb r up r-12345678 --body "Revised rule" --as "$AGENT" --json
kb rule retire r-12345678 --as "$AGENT" --json
kb r ls --all --full --json            # retired rows and full bodies

# One rules document inherited by every project. No board selector belongs here.
kb r new "Never store credentials in Kanban." --global --as "$AGENT" --json
kb r ls --global --json
kb r cat g-12345678 --global --json
kb ev --global --rule g-12345678 --json  # audited global history

# Explicit board tags: ALL is the default; includes/exclusions are repeatable.
kb r new "Kanban only." --global --board kanban --as "$AGENT" --json
kb r new "All except project-a." --global --except-board project-a --as "$AGENT" --json
kb r up g-12345678 --global --board kanban --as "$AGENT" --json
```

The first line is the headline. `kb ctx <task>`, every successful new claim and
every accepted handoff carry the complete active table of contents: global rules
first, then project rules, each with scope, id, headline, byte size and whether
more body exists.
Rendered context marks them `[g]` and `[p]`. Read a long body with `kb r cat ID`
and add `--global` for a global id; do not load every detail speculatively. Other
commands do not repeat rules—the injection boundary is claim/resume, which saves
tokens and preserves existing JSON shapes. A claim receipt keeps its existing
fields at the top level and adds `rules`; `kb t cat`'s stored claim deliberately
does not pretend it re-read the board's current rules.

Global rules live once in `registry.db`; `--global` with an explicit board
selector is refused because those name different scopes. Rules are **audited
and retire-only**. Updates retain the previous body in the
event trail; retirement removes a rule from active claims, contexts and the web
board without deleting history. There is no `rm` alias.

This is not the secret or long-form memory store. Keep credentials, secrets,
long explanations and cross-machine knowledge in the versioned/git-crypt'd
dotfiles, and let a short rule point there when needed. **Never put a secret
value in the plaintext board database.**

Every global rule carries `boardTags`. `ALL` is explicit and is the default,
`ONLY:<name>` entries form a named include set, and `EXCEPT:<name>` entries
subtract from `ALL`. Operators pass repeatable `--board NAME` and
`--except-board NAME`; the CLI validates exact registered names and refuses
ambiguous combinations. Injection and the web page include only rules whose
board tags match the addressed project, saving irrelevant context tokens. See
ADR-020.

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

## Sitreps — where a lane stands, cheaply

```bash
kb sr new "Retry path is the culprit; fix is in the queuer, tests still red." \
  --as "$AGENT" --lane driver-2 --json          # --task <ID> optional
kb sr ls --lane driver-2 --json                 # the current view, newest first
kb sr ls --lane driver-2 --all --json           # including what it superseded
```

**No task, no lease, no ceremony** — that is the whole point. A note needs a
task; a checkpoint needs a task *and* a live lease. Work done across tasks,
between them, or before anything is claimed had nowhere to go, so it went into a
reply that scrolls away.

**Post often.** This is the one record here cheap enough to write twenty times a
day, and it is what a successor reads when there was no time to write a handoff.
`kb ctx <id>` carries the sitreps that mention a task, so they reach a resuming
agent without anyone going looking.

**Old entries archive themselves.** Posting retires everything past the newest
ten in that lane. Archived sitreps are hidden from the default read and returned
by `--all` — **nothing is ever deleted**, and archiving is per lane, so another
driver's chatter cannot push yours out of view.

Provenance rides along: worktree, branch, HEAD, root HEAD, dirty count are
captured from where you ran it. "Tests green" that does not say which checkout is
a claim nobody can check.

**A sitrep is not a handoff, and not a task status.**

| | what it is | costs |
|---|---|---|
| `kb sr new` | where this lane stands right now | one command |
| `kb cp` | a resumable point on a task you hold | a lease |
| `kb h new` | *I am leaving, here is everything* | releases the lease, names a successor |

A task's **status** is a workflow state (`todo`, `in_progress`) and is always the
`--status` flag. A **sitrep** is prose about a lane and is always the `sitrep`
command. The old `status` command has no deprecated alias and fails closed.

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

**A `draft` is not work yet, and neither is anything under it.** It is the state
before `backlog`: a row still being written, whose title, body or scope may still
be wrong. `claim --next` skips it however urgent its priority, and naming it
explicitly is refused.

Because a plan is an epic, a drafted plan holds back its whole tree: no task
beneath it is offered or granted until the plan is opened, however deep it sits.
Drafts stay **visible to every driver** — a draft is hidden from the queue, not
from the reader — so anyone can read a plan being written and nobody can start
on it by accident.
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

## Tags — which part of the system this is about

**Tag your rows.** A board that cannot say whether a task is infra, queuer or
askie makes you read titles to find out, and you are the one who knows.

```bash
kb tag ls --json                                   # the vocabulary, with use counts
kb tag new infra --description "hosts, containers, deploys" --as "$AGENT" --json
kb t new "Retry backoff" --tag queuer --tag infra --json
kb t up <id> --tag queuer --as "$AGENT" --json     # replaces, does not append
kb t up <id> --clear-tags --as "$AGENT" --json     # the only way to say "none"
kb t ls --status todo --tag queuer --json          # open work in one subsystem
```

**Read `kb tag ls` before you tag.** The vocabulary is a per-board **master
file**: only a registered tag can be attached, and attaching an unregistered one
is refused naming the nearest match. That refusal is the feature — it is what
stops `infra`, `Infra` and `infrastructure` becoming three answers to one
question.

**If nothing fits, register it** with a description, then use it. Do not leave
the row unfiled and do not smuggle the subject into the title. Registering is one
command and it is paid once per concept, by whoever names it first.

Names are lowercase letters, digits and inner hyphens. `Infra` is refused rather
than folded — folding would decide for you which spelling you meant.

Tags go on **every row type**, drafts and epics included: a plan belongs to a
subsystem as much as the task it produces does.

**Tags are not lanes.** `lane` is *who picks this up* and `claim --next` routes
on it; a tag is *what part of the system this touches*. Putting a subsystem in
`lane` silently changes which driver receives the work.

Retiring a tag rows still carry is refused and says how many; `--force` strips it
from them and records the count in the trail.

## Provenance — where and when work happened

Recorded automatically. You do not pass it, and you should not have to:

- **A claim** records the worktree it was taken in, whether that worktree is a
  lane (`linked`) or an ordinary checkout (`main`), the branch, the HEAD sha,
  and — for a checkout nested inside a superproject — the **root** commit of the
  outermost repository. That last one is what says which revision of the whole
  tree was checked out; a submodule's own sha does not.
- **Checkpoints and handoffs** fill `repoPath`, `branch`, `headSha`,
  `dirtySummary` and `rootHead` the same way. An explicit `--repo` / `--branch` /
  `--head` / `--dirty` still wins: capture is a default, not an override.
- **Timestamps** are on every row already — `createdAt`, `updatedAt`,
  `completedAt`, `claimedAt`, `heartbeatAt`, `expiresAt`, `acceptedAt`,
  `resolvedAt`.
- **`kb ev`** is the audit trail: every mutation, its actor, and what changed.
  Pass `--as` to `kb t new` so the row's creation is attributable — without it
  the trail records the creation with no actor, which is honest but useless.

Run outside a git repository and provenance is recorded as absent rather than
invented, and the command works exactly the same.

## Reading

```bash
kb ctx <id> --json              # the bounded cold-start packet for a resuming agent
kb dash --json                  # per-board counts, incl. openAttention + pendingHandoffs
kb ev --task <id> --json        # the durable audit trail
kb stale --json                 # work that overran its stale budget
kb doctor --json                # integrity, orphaned rows, unreachable roots
kb archive --older-than-days 90 --as system@archive --json
kb w repoint --json             # after moving a repo: point its registered roots at it
```

`ev` is machine-written and append-only — `task_created`, `task_moved`,
`lease_seized`, `handoff_created`, `attention_raised`, `attention_resolved` and
so on. It records **what happened**; `att` records **what needs George**. Use
`ev` to reconstruct history, `att` to find open questions.

`ctx` is bounded and says so: `truncated` is computed, never assumed, and the
marker survives the truncation it describes.

`--limit` must be zero or more. A negative one reads as *no limit* in SQL, so it
is refused rather than silently handing back everything you asked to bound.

## Archival — bounded hot indexes, intact history

```bash
kb archive --older-than-days 90 --as system@archive --dry-run --json
kb archive --older-than-days 90 --as system@archive --json
kb t ls --all --json             # active plus cold tasks
kb ev --task <id> --all --json   # cold audit history for one task
```

The sweep archives only settled rows older than the cutoff: `done`/`cancelled`
tasks without a lease, their notes/checkpoints/tags/events, settled handoffs,
resolved attention and linked sitreps. Old taskless settled records are included.
Rows remain in the same backed-up SQLite board and `--all` reads them; nothing is
deleted. Operational secondary indexes contain only `archived=0` rows, so their
size follows current work rather than the board's lifetime.

Reads never run retention implicitly. The nightly backup timer explicitly sweeps
every present registered board at 90 days before snapshotting it. `--dry-run`
executes the real transaction, reports its counts, and rolls it back. See ADR-021.

## The web view

`https://kb.geoy.ws` — every board at once, behind basic auth. Read-only: it
renders what the CLI reads and writes nothing.

- **Needs you** (the landing page) — every open attention item across every
  board, oldest first, with its kind, who raised it and how long it has waited.
- **Lanes** — the counterpart: what every lane last reported, newest first.
- **Boards** — the `kb dash` projection as a table.
- **Plans** — draft epics with their bodies, each naming the work it holds back.
- **Task detail** — notes, checkpoints, the event trail, and the provenance of
  whoever holds it. Never the lease token: that is a capability, and a page that
  rendered one would hand it to whoever loaded the page.

It is `kanban serve` on loopback 14200, kept up by `kanban-serve.service` and
fronted by nginx. It binds `127.0.0.1` and has **no `--bind` flag** — kanban
implements no authentication and trusts the edge, so the only correct value is
the default. Updating is `install` then `systemctl restart kanban-serve`; the
MCP server's in-place swap does not apply to an HTTP server.

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
- A tag that is not in the board's master file is refused on attach **and on
  filter**. `kb t ls --tag infr` does not answer "nothing" — an empty list reads
  like a finding, and that is how a typo becomes a wrong answer somebody acts on.
- `--tag` and `--clear-tags` together are refused rather than ranked, like every
  other pair of answers to one question.

## Reference

`docs/adr/` in the kanban repo carries the reasoning. Most load-bearing:
ADR-008 (fail closed), ADR-010 (adapters generated from the surface),
ADR-011 (MCP server + in-place reload), ADR-012 (session handoffs and
attention), ADR-013 (plans are epics), ADR-015 (tags are a master file),
ADR-016 (the web view), ADR-017 (sitreps), ADR-018 (project rules).
ADR-019 adds the single-copy global rules inherited at claim/resume boundaries.
ADR-020 adds explicit `ALL`, named-only and all-except board targeting.
ADR-021 keeps settled history while removing it from operational indexes.
