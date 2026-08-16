# atmux import probe — 2026-08-16

**Verdict:** Passed in an isolated temporary Kanban data directory. This was a
read-only migration probe, not the production cutover.

## Source

- Repository: `/root/work/src/atmux`
- Database: `.atmux/state.db`
- Source size: 6,033,408 bytes
- Importer opened the source with SQLite read-only mode.

## Receipt

```json
{
  "epics": 114,
  "stories": 91,
  "tasks": 1138,
  "imported": 1343,
  "danglingDependencies": 1,
  "missingParents": 0,
  "registryIntegrity": "ok",
  "boardIntegrity": "ok"
}
```

The dangling reference is an existing atmux condition: task `t-ca78326b`
references deleted task `t-be01fc89`. Kanban retained that ID in
`metadata.legacyDanglingDependencies` and did not create an invalid foreign-key
edge.

## Scope still pending

- Native epic/story transition parity and side effects.
- atmux reader/writer adapter and dual-write prevention.
- Production board backup, import, row-level comparison, and rollback marker.
- Observation period before removing atmux's duplicate Kanban implementation.
