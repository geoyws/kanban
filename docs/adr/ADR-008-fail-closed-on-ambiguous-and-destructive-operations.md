# ADR-008: Fail closed on ambiguous input and destructive operations

**Status:** Accepted
**Date:** 2026-08-19 (amended 2026-08-20)
**Deciders:** George

## Context

Kanban is written for agents that cannot notice a mistake. A model turn issues
a command, reads the exit status, and moves on. Anything the CLI accepts
quietly becomes part of the durable record, and nothing later in the pipeline
re-reads it with fresh eyes.

An audit of the 0.3 binary found six defects that share one shape: an operation
that should have refused instead succeeded, and reported success.

- **A mistyped flag was a silent no-op.** `--projct alpha` was parsed, stored,
  and never read. Board selection fell through to working-directory resolution
  and wrote to whichever board happened to contain the process. `--statis done`
  listed the entire board instead of one status.
  [ADR-007](ADR-007-global-project-addressing.md) claims "a mistyped project
  name is a refusal rather than a wrong-board write"; that held for a bad
  *value* and not for a bad *flag*, which is the likelier slip.
- **`kanban init` below a registered root created a second board.** Tasks added
  from the subdirectory resolved to the nearer board and were invisible from
  the project root. Both boards reported success and neither mentioned the
  other.
- **`task move` and `task remove` deleted a live claim row.** The lease is an
  agent's authority to write; voiding it from outside left the holder to
  discover the loss when its checkpoint failed, after the work was done.
- **`context` reported `truncated: false` unconditionally.** Past the note cap
  the packet silently dropped the oldest history while telling the resuming
  agent it held the complete record. Bounded cold-start context is the
  product's central promise, and the field that qualifies it was a constant.
- **`--lease-minutes` overflowed the conversion to milliseconds.** A panic in
  debug; in release, a wrap to a negative lease.
- **`--db PATH` re-permissioned the containing directory to 0700.** Kanban
  narrowed a directory it did not create. `KANBAN_DB=/tmp/board.db` run as root
  locks `/tmp` away from every other process on the host. Separately, board
  files were created 0644 and narrowed afterwards, leaving a window in which
  any local user could open one and keep the descriptor.

## Decision

**An operation that cannot be unambiguously interpreted is refused, not
guessed.** Every flag is declared per command and an unrecognized one is an
error naming the nearest accepted flag. Silent acceptance is never the
fallback, because the caller has no way to detect it.

**An operation that destroys or overrides another agent's state requires
`--force`, and records the override.** This covers `task move` and
`task remove` against a live lease, and `kanban init` inside an enclosing
project. The refusal names the holder or the enclosing project and prints the
command that does what the operator most likely meant — `workspace attach` for
a nested init. Forcing is a first-class, audited path, not a workaround: a
seizure writes a `lease_seized` event, and a removal records the count of notes
and checkpoints it discarded.

**Short names are an exact-match table, never inferred.** The crate installs
`kanban` and `kb`, and commands carry short forms (`t ls`, `t mv`, `t rm`). Every
one is written down. Prefix inference is refused in both directions: `--proj` is
not accepted as `--project`, and `task li` is not accepted as `task list`,
because adding a `--projection` or a `task link` later would silently retarget
callers that already work — the same silent change of meaning as a mistyped flag,
arriving through a feature instead of a slip. An unknown flag close to a real one
is *suggested*, which costs nothing because the command still fails.

**A field that qualifies the data is computed, never assumed.** `truncated` is
derived by over-fetching one row past each cap. A constant that happens to be
right most of the time is a lie the rest of the time.

**Destroying state Kanban did not create is out of scope.** `--keep` prunes
only the stamped snapshots under the managed backups root; a snapshot directory
reached through `--output` belongs to the operator and is left alone. Deleting
from a path someone else chose is the same overreach as re-permissioning one.

**Kanban re-permissions only the tree it owns.** Directories it creates are
0700 from creation; the private data root is asserted to 0700 because Kanban
owns it outright. A directory the operator pointed at with `--db` is never
modified. Database and snapshot files are created 0600 by `O_EXCL` before
SQLite opens them, so there is no window in which they are public.

**`synchronous=FULL`, not `NORMAL`.** A checkpoint that survives the agent but
not the host is not durable, and resumption is the reason this ledger exists.
Write volume is a handful of rows per model turn, so the extra fsync is not a
meaningful cost.

## Consequences

Some previously-accepted command lines now fail. That is the point: each one
was already doing something other than what it said. The break is loud and
names its own fix, which is the failure mode an unattended agent can act on.

`--force` concentrates the destructive paths behind one reviewable flag, so
"which commands can destroy work state" is answerable by grepping for it.

Flag declarations must be extended whenever a command gains a flag, or the new
flag is rejected. The e2e suite exercises the accepted set for each command it
covers, so a missing declaration fails the gate rather than reaching an
operator.

Deliberately not decided: changing the default output format. Every command
still prints JSON whether or not `--json` is passed. Making the default
human-readable would break existing consumers for a cosmetic gain, and no
safety property depends on it.

## Amendment — 2026-08-20: a documented precondition is an enforced one

`restore` replaces the live registry and every board file from a snapshot. It
refused without `--force`, verified the snapshot's integrity first, and wrote a
rescue copy of what it was about to overwrite — and then told the operator to
"stop every kanban process first" while enforcing nothing. Nothing stopped it
renaming a database file out from under an open SQLite connection: readers keep
serving the unlinked inode, the next writer commits into a file no longer
reachable by name, and the restore reports success over both.

**A precondition the operator is told to satisfy is enforced by the program.**
Prose in an error message is not a guard. A `--force` flag that implies more
safety than it delivers is worse than no flag, because the operator stops
checking for themselves.

The data root now carries one advisory lock (`.lock`, an flock). Board commands
take it shared and never exclude each other, so a swarm of agents is unaffected;
`restore` takes it exclusively and refuses immediately when anything else holds
the root, naming what to do. A board command that meets a restore in progress
waits the same five seconds SQLite's `busy_timeout` gives it and then refuses
rather than read a half-replaced root.

The lock covers the data root and only the data root. A board addressed straight
by path outside it (`--db /tmp/scratch.db`) takes no lock and creates no data
root, because conjuring a private directory to lock is the same overreach as
re-permissioning one — while a `--db` path that resolves *inside* the root is
covered, traversal included.

**A numeric field with no band is not validated, it is undefined.** `priority`
accepted any `i64`. `claim --next` hands work out in ascending priority, so a
negative value took the head of every queue permanently — nothing can outrank
the bottom of the range — and no value in the type had a stated meaning. The
band is `0` (most urgent, the tier driver-only work already sorts on) through
`9`, default `3`; it follows what the ledger already meant by the field rather
than imposing a new scale on it. Only a value a caller supplies is checked: a
row holding something out of band, from an atmux import or a board written
before this rule, keeps it. Validating input is not a licence to rewrite
recorded history to match.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — durable resume contract
- [ADR-003](ADR-003-private-multi-project-personal-work-system.md) — private storage
- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — compiled-binary E2E
- [ADR-007](ADR-007-global-project-addressing.md) — board selection chain
