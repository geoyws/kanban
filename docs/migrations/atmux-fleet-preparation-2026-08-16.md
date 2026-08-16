# atmux fleet preparation receipt — 2026-08-16

This is a non-activating migration receipt for the operator's private Kanban.
SQLite remains healthy and authoritative in each atmux project until a durable
backend marker is activated with writers stopped. No legacy state was deleted.

## Prepared projects

| Project | Source | Epics | Stories | Tasks | Receipt |
| --- | --- | ---: | ---: | ---: | --- |
| atmux | SQLite | 114 | 91 | 1,138 | `/root/work/src/atmux/.atmux/backups/kanban-cutover/2026-08-16T03-00-35.340Z/receipt.json` |
| Unum | SQLite | 45 | 61 | 1,105 | `/root/work/unum/src/root/.atmux/backups/kanban-cutover/2026-08-16T03-53-35.242Z/receipt.json` |
| RentX | SQLite | 13 | 19 | 276 | `/root/work/ifca/src/rentx-root/.atmux/backups/kanban-cutover/2026-08-16T03-53-38.533Z/receipt.json` |
| MemberX | SQLite | 9 | 41 | 40 | `/root/work/ifca/src/mx-root/.atmux/backups/kanban-cutover/2026-08-16T03-53-41.477Z/receipt.json` |
| AuditX | JSON | 2 | 5 | 50 | `/root/work/ifca/src/auditx-root/.atmux/backups/kanban-cutover/2026-08-16T03-53-22.132Z/receipt.json` |
| IFCA docs | SQLite | 0 | 0 | 24 | `/root/work/ifca/src/ifca-docs/.atmux/backups/kanban-cutover/2026-08-16T03-53-44.484Z/receipt.json` |
| CRM | SQLite | 1 | 0 | 8 | `/root/work/ifca/src/crm-react/.atmux/backups/kanban-cutover/2026-08-16T03-53-51.241Z/receipt.json` |

The prepared total is 3,042 imported rows. The aggregate dashboard also holds
Kanban's own four project tasks, for 3,046 rows across eight private boards.
Every preparation reported a healthy registry and board integrity `ok`.

## Preserved source anomalies

- atmux has one dangling task dependency.
- Unum has three dangling task dependencies.
- CRM has eight tasks referring to an absent legacy epic. Their original parent
  IDs are retained in task metadata; the importer does not create corrupt
  foreign keys.
- No prepared source reported a non-terminal completion timestamp. Import code
  nevertheless preserves and reports that anomaly instead of silently erasing
  it.

## Activation gate

Activation was not attempted. Read-only process inspection found live atmux
writers for several prepared projects, including atmux, Unum, RentX, MemberX,
IFCA docs, and CRM. The atmux driver-2 lane also has unrelated dirty work.

Before activation:

1. checkpoint and hand off live work through Kanban;
2. stop atmux and orchd writers for the target project;
3. verify the source hash still matches its preparation receipt, or prepare a
   fresh isolated board if it changed;
4. activate the durable marker and restart consumers on the external-aware
   atmux build;
5. prove reads and writes through the real CLI and observe that no legacy
   task/story/epic state changes; and
6. retain rollback until the first external write, after which rollback must
   refuse rather than discard new durable work.

Duplicate atmux Kanban code and tables remain until every project has passed
this gate and the observation period.
