---
name: kb
description: Use the authoritative Kanban work ledger on HAX over SSH — tasks, claims and leases, checkpoints, handoffs, sitreps, and attention items that need George. Use whenever recording, reading, resuming, or handing over durable work state.
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

## HAX is the execution boundary

The authoritative Kanban registry and boards live on **HAX**. Before any `/kb`
read or write, check the current host **before attempting SSH**:

```bash
hostname                   # if this prints exactly "hax", stay in this shell
command -v kb              # on hax, must resolve the installed HAX binary
kb v                       # on hax, verify the installed binary
```

If `hostname` prints anything other than exactly `hax`, enter HAX and verify the
remote boundary there:

```bash
ssh hax
hostname                   # must now print exactly "hax"
command -v kb              # must resolve the installed HAX binary
kb v
```

A non-interactive `ssh hax 'kb ...'` does **not** load HAX's interactive
`PATH`, so it can report `kb: command not found` even though the binary is
installed. Do not rediscover or re-quote this on every call. From outside HAX,
use the bundled argv-preserving wrapper for one-shot commands:

```bash
<skill-dir>/scripts/hax-kb v
<skill-dir>/scripts/hax-kb tag ls --project kanban --json
<skill-dir>/scripts/hax-kb search "status change" --project kanban --json
```

The wrapper verifies that the remote host is exactly `hax`, invokes the fixed
installed path `/root/.local/bin/kb`, and preserves spaces and shell
metacharacters as literal arguments. Use an interactive `ssh hax` session for a
related series of commands or prose-heavy mutations; use the wrapper for
deterministic one-shots. Never fall back to a local `kb` after either path
fails.

Every `kb …` example below is a command to run **inside the verified HAX
shell**. Keep one SSH session open for related operations to save connection
overhead and tokens. Never run `ssh hax` from a shell whose exact `hostname` is
already `hax`; that shell is already at the required boundary.

From another machine, never invoke a local `kb` or read a local Kanban SQLite
file as a fallback. If HAX SSH or the installed HAX binary is unavailable, stop
and report that boundary as blocked; do not run `kb init`, create a replacement
board, or let local state diverge.

Prefer `--project NAME` for remote commands because the caller's local checkout
path may not exist on HAX. Use `--workspace PATH` only with a path verified to
exist and be registered on HAX. For one-shot automation, shell-quote the entire
remote command and every dynamic value safely; an interactive SSH session is
preferred for prose mutations so titles and bodies cannot be split or expanded
by an intermediate shell.

Selector rule: board-owned commands such as `task`, `tag`, `search`, `claim`,
and `attention` should normally use `--project NAME` remotely. Registry-owned
`rule` commands must not receive `--project`, `--workspace`, or `--db`; rules
live once in the registry and select boards through `ALL`, `ONLY:<board>`, and
`EXCEPT:<board>` tags. In particular, use `hax-kb r ls --json`, never
`hax-kb r ls --project kanban`.

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
| `workspace` | `ls`=list `att`=attach `det`=detach |
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
kb ws det --root /retired/worktree --as "$AGENT" --json
kb ws ls --all --json           # including detached aliases
```

## Tag-scoped rules — what frames work

Put short, non-secret operating constraints in the one registry-owned rules
document. A board owns work; it is a selector tag on a rule, not a rule scope:

```bash
kb r new "Universal rule." --as "$AGENT" --json       # tags: ALL
kb r new --body-file /tmp/non-secret-rule.md --as "$AGENT" --json
kb r ls --json                         # active table of contents, oldest first
kb r cat r-12345678 --json             # one full body, fetched lazily
kb r up r-12345678 --body "Revised rule" --as "$AGENT" --json
kb rule retire r-12345678 --as "$AGENT" --json
kb r ls --all --full --json            # retired rows and full bodies
kb ev --rule r-12345678 --json          # audited history

# ALL is the default; includes/exclusions are repeatable selector tags.
kb r new "Kanban only." --board kanban --as "$AGENT" --json
kb r new "All except project-a." --except-board project-a --as "$AGENT" --json
kb r up r-12345678 --board kanban --as "$AGENT" --json

# Lowercase subsystem tags intersect the board selector.
kb r new "Queuer only." --tag queuer --as "$AGENT" --json
kb r up r-12345678 --clear-tags --as "$AGENT" --json
```

The first line is the headline. `kb ctx <task>`, every successful new claim and
every accepted handoff carry the applicable active table of contents. Each
summary has one `tags` array plus id, headline, byte size and whether more body
exists. Read a long body with `kb r cat ID`; do not load every detail
speculatively. Other commands do not repeat rules—the injection boundary is
claim/resume, which saves tokens. A stored claim deliberately does not pretend
it re-read current rules.

Rules live once in `registry.db` and are **audited and retire-only**. Updates
retain the previous body; retirement removes a rule from active injection and
the web view without deleting history. There is no `rm` alias. `--global` is
retired and explicitly refused; `g-*` remains valid only as a historical ID.

This is not the secret or long-form memory store. Keep credentials, secrets,
long explanations and cross-machine knowledge in the versioned/git-crypt'd
dotfiles, and let a short rule point there when needed. **Never put a secret
value in the plaintext board database.**

Every rule carries one `tags` array. `ALL` is explicit and default,
`ONLY:<name>` is a named include, and `EXCEPT:<name>` subtracts from `ALL`.
Repeatable `--board`/`--except-board` flags validate exact board names.
Lowercase `--tag` selectors must exist on an active board; several are an OR
set intersected with the board selector. Task claim/context/handoff injection
requires a matching task tag. Taskless session handoffs and web board pages
omit subsystem-scoped rules. See ADR-027.

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

## Search and bounded RAG context

Search before opening many cards speculatively. Results cover board tasks,
notes, checkpoints, handoffs, attention, sitreps and selected audit events,
plus the one registry rules document. Each result has a stable
`kanban://BOARD/KIND/ID` or `kanban://rules/rule/ID` citation.

```bash
kb search "resume the release handoff" --project kanban --json
kb search t-12345678 --source task --limit 5 --max-chars 4000 --json
kb search "authentication recovery" --tag auth --all-boards --json
kb search "retired decision" --all --json       # include archived history
```

Use the returned snippets to choose sources, then read the cited task, rule, or
trail through the ordinary narrow commands. Do not treat a similarity score as
proof and do not cite a snippet without its source URI. `--limit` is 1–100 and
`--max-chars` is 256–100000; use the smallest useful bounds. Filters are
`--source`, `--status`, `--tag`, `--lane`, `--after`, and `--before`.

`search` is read-only. It can calculate a missing deterministic local vector in
memory, but it never writes the cache as a side effect. Cache refresh is an
explicit audited maintenance operation:

```bash
kb search-rebuild --project kanban --as system@search-index --json
kb search-rebuild --all-boards --as system@search-index --json
```

MCP exposes these as `search` (read-only) and `search_rebuild` (write). The web
view at `/search` is cross-board and read-only. `kb doctor --json` reports
`searchIndex` parity and cache freshness for every board.

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
kb h acc <id> --as driver-2 --json      # task lease when claimable; acknowledgement only when settled
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

To inspect the same scheduler queue without taking a lease:

```bash
kb claim --candidates --as "$AGENT" --project NAME \
  [--lane LANE] [--role ROLE] [--caller-scope driver] \
  [--no-cross-lane] [--allow-reassign] [--tag NAME] [--limit N] --json
```

Candidate inspection is strictly read-only and never returns lease tokens.
It shares eligibility and ordering with `claim --next`; claim the selected ID
atomically before starting work because inspection does not reserve it.

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

**Use this tree for durable todo lists.** For a multi-item roadmap, make the
roadmap an epic and every top-level todo item a direct child epic. Put the
actionable stories and tasks beneath that child, and use dependencies between
child epics for ordering. The roadmap body records scope and success criteria;
do not maintain a duplicate Markdown checkbox list. Any rendered checklist or
progress count is a projection of the board.

The child epic is the roadmap item's checkbox. The current CLI does not
generically derive epic completion from all descendants, so move a child epic
to `done` only after every non-cancelled descendant is settled and its evidence
is durable. A single standalone action stays a task; do not wrap it in
ornamental epics.

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

**Only a task is claimable.** An epic and a story are containers;
`claim --next` skips them, and naming one explicitly is refused pointing at
`story advance` or at the children. Story gates project story status, while
generic epic completion is still an explicit, agent-verified transition.

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
  Pass `--as` to `kb t new`; compatibility calls that omit it are explicitly
  attributed to `system@cli`, never stored with a blank post-migration actor.
- **`kb audit verify --json`** verifies every board and registry hash chain.
  Keep a snapshot `manifest.json` outside the HAX data root when rollback
  evidence matters, then run `kb audit verify --against manifest.json --json`.
  A chain proves continuity; the retained manifest is what proves freshness.

Run outside a git repository and provenance is recorded as absent rather than
invented, and the command works exactly the same.

## Reading

```bash
kb ctx <id> --json              # the bounded cold-start packet for a resuming agent
kb dash --json                  # per-board counts, incl. openAttention + pendingHandoffs
kb ev --task <id> --json        # the durable audit trail
kb ev --registry --json         # registry rules and workspace lifecycle
kb stale --json                 # work that overran its stale budget
kb doctor --json                # integrity, orphaned rows, unreachable roots
kb audit verify --json          # all hash chains, including archived history
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
It also archives terminal deployment attempts that are no longer the current
verified success for their `(repo, tier, environment)`. Started attempts and
each target's latest verified success always remain hot.
Rows remain in the same backed-up SQLite board and `--all` reads them; nothing is
deleted. Operational secondary indexes contain only `archived=0` rows, so their
size follows current work rather than the board's lifetime.

Reads never run retention implicitly. The nightly backup timer explicitly sweeps
every present registered board at 90 days before snapshotting it. `--dry-run`
executes the real transaction, reports its counts, and rolls it back. See ADR-021.

## Deployment attempts — exact release receipts

```bash
kb deploy start --repo OWNER/REPO --commit FULL_40_CHAR_SHA --tier @_p \
  --environment production --host hax --url https://service.example \
  --task TASK_ID --operation-id OPERATION_ID --as "$AGENT" --json

kb deploy finish DEPLOYMENT_ID --token CAPABILITY_TOKEN --result succeeded \
  --phase verification --served-commit FULL_40_CHAR_SHA \
  --receipt "what was checked live" --as "$AGENT" --json

kb deploy current --json
kb deploy list --status failed --json
kb deploy list --all --json
```

Canonical tiers are `@_bdt`, `@_bd`, `@_bst`, `@_bs`, `@_s`, `@_uat`, and
`@_p`. Record the full pushed commit. `succeeded` is refused unless the served
commit matches it exactly and the phase is `verification` with a non-empty live
receipt. A retry starts a new row with `--retry-of`; never rewrite the old
attempt. Keep the start receipt's capability token until finishing. Use
`deploy abandon --token … --note …` when no failure was observed; `--force` is
an explicit audited recovery override. See ADR-030.

## The web view

`https://kb.geoy.ws` — every board at once, behind shared Google SSO with only
`geoyws@gmail.com` allowed. Reads use the same Store as the CLI. The sole shipped
write is the Needs-you reply/resolve action, attributed to `geo`.

- **Needs you** (the landing page) — every open attention item across every
  board, oldest first, with its kind, who raised it, how long it has waited, and
  an inline reply plus quick decision buttons. A reply resolves the item and is
  preserved as its resolution note.
- **Lanes** — the counterpart: what every lane last reported, newest first.
- **Boards** — the `kb dash` projection as a table.
- **Plans** — draft epics with their bodies, each naming the work it holds back.
- **Deployments** — verified current releases, active attempts, recent failures,
  and immutable per-attempt receipts; the existing WebSocket refresh keeps it live.
- **Search** — cited exact, lexical, and semantic retrieval across every board.
- **Task detail** — notes, checkpoints, the event trail, and the provenance of
  whoever holds it. Never the lease token: that is a capability, and a page that
  rendered one would hand it to whoever loaded the page.

It is `kanban serve` on loopback 14200, kept up by `kanban-serve.service` and
fronted by nginx. It binds `127.0.0.1` and has **no `--bind` flag** — kanban
implements no authentication and trusts the edge, so the only correct value is
the default. Updating is `install` then `systemctl restart kanban-serve`; the
MCP server's in-place swap does not apply to an HTTP server. `/live` upgrades to
a WebSocket and sends revision-only refresh notifications; agent CLI/MCP access
does not depend on that socket.

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
ADR-016 (the web view), ADR-017 (sitreps), ADR-018 (the original board-local
rules). ADR-027 supersedes the scoped rule decisions with one registry-owned,
tag-scoped rules document using `ALL`, `ONLY:<board>`, `EXCEPT:<board>` and
lowercase subsystem tags.
ADR-021 keeps settled history while removing it from operational indexes.
