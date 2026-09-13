# ADR-045: A sprint is a typed row that scopes claims and cannot close without a served version

**Status:** Draft — pending acceptance
**Date:** 2026-09-11
**Authority:** sprint epic `e-ee95dbd5` is the authoritative requirement source
and overrides weaker draft wording here. This ADR records the implementation
decision for review; it does not claim operator approval.
**Supersedes:** nothing.

## Context

The board has no sprint concept. `claim --next` offers the next wise row by
priority, which is exactly right inside a sprint and exactly wrong across one:
nothing stops a lane from claiming past a release boundary that was never
crossed. The observed failure (epic `e-ee95dbd5`): the release boundary never
gets crossed, because no row on the board says "this is the sprint, this ships
as vX.Y, and it is not done until that version is served".

What exists to build on: typed rows already have their own tables with typed
id prefixes (`a-` attention, `d-` deployments, `sub-` subscriptions), the
schema ladder is append-only (`BOARD_V27` next, board schema 26 today), every
write goes through the store with ADR-008 refusals, every mutation is an
event in the hash chain (ADR-029), and `claim`'s candidate pool already has a
lane/no-cross-lane vocabulary this must extend rather than duplicate.

## Decision

### 1. A sprint is a row in its own table, id prefix `sp-`

```sql
CREATE TABLE sprints (
  id TEXT PRIMARY KEY,            -- 'sp-xxxxxxxx'
  title TEXT NOT NULL,
  body TEXT,                      -- goal + success criteria, written by `sprint plan`
  status TEXT NOT NULL DEFAULT 'planned'
    CHECK(status IN ('planned','current','closed','abandoned')),
  target_version TEXT NOT NULL,
  scheduled_start INTEGER NOT NULL, -- planned epoch-ms boundary
  scheduled_end INTEGER NOT NULL,   -- planned; >= scheduled_start
  starts_at INTEGER NOT NULL,       -- actual, set by sprint start
  ends_at INTEGER,                  -- actual, set by close/abandon
  closed_by_deployment TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  archived INTEGER NOT NULL DEFAULT 0
);
-- Exactly one current sprint per board, enforced by the index, not by hope.
CREATE UNIQUE INDEX one_current_sprint ON sprints(status) WHERE status='current';
```

Board schema 26 → 27 (`BOARD_V27`, appended to `BOARD_MIGRATIONS`; the ladder
test keeps them equal). No `tasks` table rebuild and no `ALTER` on `tasks`:
attachment is a junction table whose PK on `task_id` holds the
one-sprint-per-task rule a column would have held, while recording when and
by whom each attachment was made,

```sql
CREATE TABLE task_sprints (
  task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
  sprint_id TEXT NOT NULL REFERENCES sprints(id),
  attached_at INTEGER NOT NULL,
  attached_by TEXT NOT NULL
) STRICT;
CREATE UNIQUE INDEX one_current_sprint ON sprints(status) WHERE status='current';
```

Production migrations fail closed. V27 uses ordinary additive CREATE and
ALTER TABLE ADD COLUMN statements without IF NOT EXISTS; an unexpected object
is incompatible state, not evidence that migration already happened. The
migration fixture constructs the actual v26 ladder and preserves a live v26
row rather than lowering user_version on a current schema.

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

### 2. Seven verbs, and the two flags that reach an existing command

```bash
kb sprint new "<title>" --target-version X.Y.Z --start EPOCH_MS --end EPOCH_MS --as ACTOR
kb sprint plan <id> --body-file P --candidate ID ... --as ACTOR
kb sprint plan <id> --body-file P --parent-epic e-... --as ACTOR
kb sprint plan <id> --body-file P --empty-scope --as ACTOR # deliberate empty scope
kb sprint start <id> --as ACTOR
kb sprint close <id> --deployment d-… --as ACTOR [--carry-to sp-… --carry-note TEXT]
kb sprint abandon <id> --note TEXT --as ACTOR     # any non-closed -> abandoned, with a note
kb sprint ls [--status …] [--all]                 # newest first; default hides closed/abandoned
kb sprint cat <id>                                # the row, its rows, its deployment

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

- kb dash publishes target version, ceiling days remaining from scheduled_end,
  actual open and done counts, and the goal headline/body for the current sprint.
- kb ctx <task> carries sprint id, target version and goal headline only when
  attached; an unattached task preserves the old text and absent JSON field.
- `sprint ls`/`cat` are read paths over the same store methods.

The web Sprint page, search citations (`kanban://BOARD/sprint/ID`) and
sprint-scoped rules are the epic's later children (`sp-web`, `sp-docs`),
deliberately not this ADR's surface.

### 6. Events: five kinds, one chain

`sprint_created`, `sprint_planned`, `sprint_started`, `sprint_closed`
(payload: the `d-` id and the version it served), `sprint_abandoned` (payload:
the note), and `task_sprint_changed` (payload: old and new sprint ids) — all
through the existing `event()` writer, inside the same transactions as the
row writes, covered by `audit verify` like every other mutation. `sprint ls`
and `cat` are reads and write nothing.

## Consequences

- `COMMANDS` gains the `sprint` command with seven subcommands and the three
  flag additions (`--sprint` on `t new`/`t up`/`claim`, `--any-sprint` on
  `claim`); `schema --json` and the MCP tools follow from the table (ADR-010)
  with no hand-written tool.
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
- [ADR-030](ADR-030-deploy-attempts-are-capability-receipts.md) — the deployment rows the close gate reads
- [ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) §2 — the schema-vs-store enforcement precedent
- `rust/db.rs` `BOARD_MIGRATIONS`, `rust/model.rs` closed sets, `rust/lib.rs`
  `COMMANDS` — the surfaces this extends
