# Integrating atmux

atmux already has production SQLite task state. Integration must preserve that
history and command behavior; it is not a big-bang replacement.

## Migration sequence

1. Extract shared contract tests for WAL setup, immediate claim transactions,
   dependency eligibility, append-only notes, and cold-start context.
2. Add an atmux adapter implementing Kanban's repository interface over the
   existing `.atmux/state.db` schema.
3. Ship `atmux task note` and structured checkpoints using the next free,
   append-only atmux migration rung.
4. Make handoff/resume render the Kanban context packet while preserving
   existing human-readable handoff artifacts.
5. Dogfood with parity checks before moving or renaming any existing table.

## Compatibility rules

- Existing task IDs, epics, stories, dependencies, owners, and timestamps are
  stable identities and must not be regenerated.
- Existing per-team databases remain valid boards. atmux passes an explicit
  board path instead of registering product workspaces globally.
- `tasks.note` may remain the closing-note compatibility field while new
  progress history appends to `task_notes`/checkpoints.
- No state database, WAL, generated TODO, or registry pointer enters a managed
  product repository.
- Cross-team dashboards read boards serially or through bounded concurrency;
  they do not hold many read transactions open at once.
