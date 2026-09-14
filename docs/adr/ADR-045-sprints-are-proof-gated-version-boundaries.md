# ADR-045: A sprint is a typed row that scopes claims and cannot close without a served version

**Status:** Implemented — core, web, sprint-scoped rules, and first-class search complete
**Date:** 2026-09-11
**Authority:** sprint epic `e-ee95dbd5` is the authoritative requirement source
and overrides weaker wording here. The typed lifecycle, claim boundary,
deployment proof gate, CLI/read projections, server-rendered sprint pages,
sprint-scoped rules, and first-class sprint search are implemented.
**Supersedes:** nothing.

## Context

Before V27, the board had no sprint concept. `claim --next` offered the next wise
row by priority, which was right inside a release boundary and wrong across one:
nothing stopped a lane from claiming past a boundary that was never crossed.
The observed failure (epic `e-ee95dbd5`) was that no durable row said "this is
the sprint, this ships as vX.Y, and it is not done until that version is served".

The historical baseline offered typed rows in their own tables with typed id
prefixes (`a-` attention, `d-` deployments, `sub-` subscriptions), append-only
migrations through board schema 26, ADR-008 store refusals, and ADR-029 hash-
chained events. V27 introduced sprint rows while extending the existing claim
candidate pool and lane/no-cross-lane vocabulary rather than duplicating them.
V28 is now the current board schema and adds sprint rows to first-class search.

## Decision

### 1. A sprint is a row in its own table, id prefix `sp-`

The relevant `BOARD_V27` DDL is:

```sql
CREATE TABLE sprints (
 id TEXT PRIMARY KEY NOT NULL,
 title TEXT NOT NULL,
 body TEXT,
 status TEXT NOT NULL DEFAULT 'planned'
  CHECK(status IN ('planned','current','closed','abandoned')),
 target_version TEXT NOT NULL,
 scheduled_start INTEGER NOT NULL,
 scheduled_end INTEGER NOT NULL CHECK(scheduled_end >= scheduled_start),
 starts_at INTEGER NOT NULL,
 ends_at INTEGER,
 closed_by_deployment TEXT,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1))
) STRICT;
CREATE UNIQUE INDEX one_current_sprint ON sprints(status) WHERE status='current';
CREATE TABLE task_sprints (
 task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 sprint_id TEXT NOT NULL REFERENCES sprints(id),
 attached_at INTEGER NOT NULL,
 attached_by TEXT NOT NULL
) STRICT;
ALTER TABLE deployments ADD COLUMN sprint_id TEXT REFERENCES sprints(id);
ALTER TABLE deployments ADD COLUMN target_version TEXT;
ALTER TABLE deployments ADD COLUMN served_version TEXT;
```

`scheduled_start` and `scheduled_end` are planned epoch-millisecond boundaries.
`starts_at` is initialized to the sentinel `0` and overwritten with the actual
start time by `sprint start`; `ends_at` records the actual close or abandon time.
The partial unique index enforces exactly one current sprint per board.

V26 is the historical pre-sprint migration baseline. V27 adds the sprint model;
V28 is current and adds sprint search schema plus a backfill of existing sprint
rows. Both migrations are appended to `BOARD_MIGRATIONS`; the ladder test keeps
the declared schema and migrations equal. No `tasks` table rebuild or `ALTER`
is needed: `task_sprints` holds the one-sprint-per-task rule while recording who
attached each row and when. The deployment `ALTER`s bind attempts to a sprint
target and record the served-version observation.

Production migrations fail closed. V27 uses ordinary additive `CREATE` and
`ALTER TABLE ADD COLUMN` statements without `IF NOT EXISTS`; an unexpected
object is incompatible state, not evidence that migration already happened.
The migration fixture constructs the actual v26 ladder and preserves a live
v26 row rather than lowering `user_version` on a current schema.

`task_sprints` carries no CHECK beyond the FKs because attachment rules are
cross-row (an epic attaches its subtree; a descendant may sit in another
sprint) and belong in the store where a refusal can name the fix (ADR-042 §2
precedent: half-enforced schema invariants read as though the schema were
authoritative).

**Not the tasks table.** A sprint is not work and must not be claimable,
gated, or handed off; putting it in `tasks` would need `type='sprint'`
exemptions in the claim gate, the story gate, the nesting rules and the
worktree contract — four refusals to write for the privilege of one table
saved. Typed rows get tables (the `attention`/`deployment`/`subscription`
precedent), and `sp-` is a first-class id like `a-` and `d-`.

**Statuses are a closed set** in `model.rs` (`SPRINT_STATUSES`), beside
`ATTENTION_STATUSES`, for the same reason: the schema publishes the
vocabulary instead of a convention growing one.

### 2. Seven verbs, and the flags that reach existing commands

```bash
kb sprint new "<title>" --target-version X.Y.Z --start EPOCH_MS --end EPOCH_MS --as ACTOR
kb sprint plan <id> --body-file P --candidate ID ... --as ACTOR
kb sprint plan <id> --body-file P --parent-epic e-... --as ACTOR
kb sprint plan <id> --body-file P --empty-scope --as ACTOR # deliberate empty scope
kb sprint start <id> --as ACTOR
kb sprint close <id> --deployment d-… --as ACTOR [--carry-to sp-… --carry-note TEXT]
kb sprint abandon <id> --note TEXT --as ACTOR     # any non-closed -> abandoned, with a note
kb sprint list [--status …] [--all]               # newest first; default hides closed/abandoned
kb sprint show <id>                               # the row, its rows, its deployment

kb t new TITLE --sprint sp-…                      # attach at creation
kb t up <id> --sprint sp-… | --clear-sprint       # attach/detach later
kb claim <id|--next> --sprint sp-… | --any-sprint # the overrides in §3
```

sprint plan requires candidate ids, a parent epic, existing explicitly
attached scope, or the explicit --empty-scope decision. sprint start
requires both a non-empty goal/criteria body and that deliberate scope record.

`sprint start` stamps `starts_at`; `close` stamps `ends_at` and
`closed_by_deployment`; `abandon` stamps `ends_at` and requires a note for the
same reason a `defer` consequence must name its trigger (ADR-042 §7): "later"
with no stated reason is how a boundary silently stops existing.

**Attaching an epic attaches its subtree** — every descendant not already
attached to another sprint. Audit emits one task-subjected attachment event
per affected row, so event visibility hides restricted descendants without
losing lineage. Detaching remains explicit and non-recursive.

Every refusal names its fix (ADR-008), the load-bearing ones:

| # | condition | refusal |
| --- | --- | --- |
| 1 | `start` when a current sprint exists | `sprint sp-a is current; close or abandon it before starting sp-b` |
| 2 | `close` with a deployment that is not `succeeded` at phase `verification` | `sprint sp-a cannot close on d-x: that attempt is <status> (<phase>); a sprint closes only on a succeeded verification-phase deployment` |
| 3 | `close` with no `--deployment` | `sprint close requires --deployment d-…: the version is served or the sprint is not done` |
| 4 | `--sprint` naming an unknown/abandoned sprint | `sprint sp-x does not exist on this board` / `sprint sp-x is abandoned` |
| 5 | `--sprint` together with `--any-sprint` | refused, like every pair of answers to one question |
| 6 | `plan`/`start`/`close`/`abandon` on a closed sprint | `sprint sp-a is closed history; its card cannot be rewritten` |

--target-version is semver-shaped X.Y.Z(-suffix)?. It is distinct from the
global --version banner. --start and --end are planned epoch milliseconds;
actual lifecycle stamps remain starts_at and ends_at.

### 3. Claims are sprint-scoped by default, and crossing is recorded

When a board has a `current` sprint, `claim --candidates` and `claim --next`
offer only rows whose `sprint_id` is that sprint. Unattached rows are **not**
offered: the sprint is the boundary, and "unattached" is outside it, not
inside. Two overrides, both recorded on the `task_claim` event payload
(`sprintOverride: "sp-…"` / `"any"`):

- `--sprint sp-…` — work another sprint's rows deliberately (a carry-over
  fix, a hotfix landed against the wrong boundary);
- `--any-sprint` — the escape hatch, for the board operator's own use, that
  says "I know" out loud in `kb ev`.

On a board with **no** current sprint the filter is a no-op and every
behaviour is byte-identical to today. The same eligibility check governs
handoff acceptance when it would mint a lease; explicit overrides are
recorded without changing token, heartbeat, or expiry semantics.

### 4. Done means served: the close gate reads a real deployment row

deploy start --sprint sp-… resolves and stores both the typed sprint id and
its target version in the deployment row under the same transaction. A
successful bound deploy finish requires --served-version X.Y.Z and stores
that typed observation; it must equal the bound target. sprint close accepts
only a succeeded verification attempt whose sprint_id, target_version, and
served_version exactly match the closing sprint. Receipts are never parsed.
If any attached row remains outside done/cancelled, close requires both a
named --carry-to sprint and non-empty --carry-note; authorization checks,
attachment moves, carry event, and close event commit atomically.

### 5. Projections: `dash` and `ctx` (the web page is `sp-web`)

- `kb dash` publishes target version, ceiling days remaining from
  `scheduled_end`, actual open and done counts, and the goal headline/body for
  the current sprint.
- `kb ctx <task>` carries sprint id, target version and goal headline only when
  attached; an unattached task preserves the old text and absent JSON field.
- `sprint list`/`show` are read paths over the same store methods.
- The server-rendered, read-only sprint projections are `/sprints` across
  boards, `/sprints/BOARD` for one board, and `/sprint/BOARD/ID` for detail.

The typed lifecycle, CLI projections, web pages, first-class retrieval, and
sprint-scoped rules are implemented. V28 indexes sprint title, body, and target
version as source `sprint`; lifecycle updates refresh the document and results
carry citations of the form `kanban://BOARD/sprint/ID`. The migration backfills
existing sprints. Sprint rows are board-only, carry no tags, and are not
automatically archived.

A sprint-scoped rule stores selector `SPRINT:sp-ID` together with exactly one
`ONLY:BOARD`, plus any optional subsystem tag intersection. The sprint must
exist on that board; closed historical sprints remain valid selectors.
`rule add --board BOARD --sprint sp-ID` sets the scope, `rule update --sprint`
replaces it, and `rule update --clear-sprint` removes it; setting and clearing
are mutually exclusive. Claim, handoff acceptance, and task context evaluate
the rule against the task's authoritative attachment. Tasks without a sprint
or attached to another sprint do not receive it. Raw rule inventory and
board-targeted search inventory are unchanged, and a board HTML projection
without a task does not show sprint-only rules.

### 6. Events: six kinds, one chain

`sprint_created`, `sprint_planned`, `sprint_started`, `sprint_closed`,
`sprint_abandoned`, and `task_sprint_changed` all use the existing `event()`
writer inside the row transaction. The close payload records `deploymentID`,
`targetVersion`, and `previousStatus`; the served-version observation remains on
the bound deployment. Abandon records its note, and attachment changes record
old and new sprint ids. `audit verify` covers the resulting chain. `sprint list`
and `show` are reads and write nothing.

## Consequences

- `COMMANDS` includes the `sprint` command with seven subcommands and the
  sprint flags on task attachment, claim/handoff acceptance, deployment proof,
  and close/carry operations. `schema --json` and the MCP tools follow from the
  command table (ADR-010), with no hand-written tool.
- `ATTENTION`-style field lists: `SPRINT_FIELDS` joins the
  field-list-drift guard.
- e2e (the `sp-e2e` row, serialized compiled-process cases): the no-sprint
  no-op, scoped candidates with the recorded override visible in `kb ev`,
  close refused without a succeeded verification deployment and accepted with
  one, the one-current-sprint uniqueness, subtree attachment, and
  `audit verify` green across a migration from schema 26.
- What is deliberately not built: velocity, points, burndown, rollover,
  scheduling automation, and any change to lease/heartbeat/checkpoint semantics.

## References

- Epic `e-ee95dbd5` (the plan; success criteria quoted in the task bodies)
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — refusals name the fix
- [ADR-029](ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md) — the chain every sprint event joins
- [ADR-030](ADR-030-deployment-attempt-ledger-and-self-archiving.md) — the deployment rows the close gate reads
- [ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) §2 — the schema-vs-store enforcement precedent
- `rust/db.rs` `BOARD_MIGRATIONS`, `rust/model.rs` closed sets, `rust/lib.rs`
  `COMMANDS` — the surfaces this extends
