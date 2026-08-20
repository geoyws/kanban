# Replacing atmux's embedded Kanban

Kanban becomes the sole owner of personal work state. atmux keeps its session,
pane, worktree, routing, and cockpit responsibilities and calls this package for
task, story, epic, progress, claim, and handoff operations.

## Migration sequence

1. Freeze an inventory and contract suite for every atmux Kanban reader and
   writer, including task/story/epic lifecycle, lanes, inbox dispatch, cron,
   hygiene, dashboard, publish, and rotation flows.
2. Port missing behavior into this repository without importing atmux process
   orchestration into the domain model.
3. Add an importer for `.atmux/state.db` and legacy `kanban.json`; compare IDs,
   row counts, relationships, status, timestamps, notes, and extension fields.
   Re-import is insert-only unless the operator confirms all legacy writers are
   stopped and uses the explicit reconciliation path.
4. Add an atmux compatibility adapter backed by the private Kanban registry.
   Canonical team roots and their driver worktrees attach to one project board.
5. Switch readers, then writers, then handoff/rotation consumers. During the
   transition, prevent dual writes from becoming two authorities.
6. Dogfood through a rollback-capable observation period and run old/new parity
   receipts against real private state.
7. Remove atmux's duplicate Kanban schemas, repositories, migrations, JSON
   writers, and Markdown handoff artifacts only after every consumer is moved.

## Parity receipts

Step 3 and step 6 both require the migration to be *checked*, not just run. An
import receipt cannot serve as that check: its `counts` describe rows read from
the source and its `created`/`updated` describe rows written to the board, which
are two numbers about two different things. Neither says the board now holds
what the source held.

`kanban import atmux-sqlite <path> --as ACTOR --verify` re-reads the source and
compares it against the board, writing nothing:

```json
{
  "verified": false,
  "compared": 4,
  "matched": 2,
  "missing": ["t-b"],
  "differing": [
    { "id": "t-a", "field": "title", "source": "Task A", "board": "TAMPERED" }
  ],
  "fields": ["type", "parentID", "title", "…", "atmuxExtra", "note"]
}
```

`verified` is true only when nothing is missing and nothing differs. The receipt
names the fields it covered, so a green result states its own scope instead of
implying it checked everything: the identities the compatibility rules below
call stable, plus the legacy note and the unknown extension fields they say must
survive. Values the import itself stamps — `updated_at`, and the normalization
metadata added only when a row raised a warning — are deliberately out of scope,
since a receipt that flagged them would be red on a faithful import.

The comparison is built from the same mapping the importer inserts through, not
a second reading of the legacy schema. Two independent mappings would agree
until the first time either changed, and drifting silently is precisely what the
receipt exists to catch.

`--verify` cannot be combined with `--reconcile`, `--force` or `--dry-run`.
Those describe a write; letting one win silently is how an operator comes away
believing a cutover was checked when it was performed.

## Compatibility rules

- Existing task IDs, epics, stories, dependencies, owners, and timestamps are
  stable identities and must not be regenerated.
- Existing per-team databases remain immutable migration sources until the
  imported Kanban board is verified and backed up.
- Reconciliation is an atomic pre-cutover refresh. It must never run while
  atmux or orchd can still mutate the legacy work tables.
- `tasks.note` and unknown extension fields must survive import even when the
  new native representation uses append-only notes/checkpoints.
- No state database, WAL, generated TODO, handoff file, or registry pointer
  enters a managed product repository.
- Cross-team dashboards read boards serially or through bounded concurrency;
  they do not hold many read transactions open at once.
- Do not delete or rewrite non-Kanban atmux SQLite tables during takeover.
