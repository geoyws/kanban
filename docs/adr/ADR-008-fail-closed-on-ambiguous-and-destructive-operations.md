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

**A destructive path found once is looked for everywhere else.** The rule
above — a live lease is not voided without `--force`, and a seizure is
recorded — was implemented in `task move` and `task remove` and left
unimplemented in `import --reconcile`, which deleted the claim rows of every
task it overwrote. Same silent void, same discovery-by-failed-checkpoint, one
command over. It now refuses by default naming every holder at once, seizes
under `--force`, and writes the same `lease_seized` event. Fixing an instance
of a defect class is not fixing the class; the grep for other call sites is
part of the fix.

**A destructive command can be asked what it would do.** `--dry-run` runs an
import in full and rolls back, so the receipt describes the real code path
rather than a second estimate of it that is free to drift. It reports the
created and updated counts, the warnings, and the leases the run would seize.

**A diagnostic never modifies what it diagnoses, and a missing board is
never quietly replaced.** Opening a board creates it, which is how a board is
made and not how a registered one should be found. A board file that has gone
missing was destroyed, so standing an empty one up in its place converts
recoverable data loss into a board that reports itself fine. `doctor` did
exactly that: it recreated the file it was asked to inspect, then certified the
empty result healthy — the health check erasing the evidence that anything was
wrong. Commands that work on one board now refuse and name both recoveries;
the commands that survey every board (`doctor`, `dashboard`, `backup`) report
the gap and continue, because dying on the first missing board helps nobody who
has to fix it, and `restore` cannot be blocked by the damage it exists to
repair.

**A health check reports what a b-tree check cannot see.** `integrity_check`
validates page structure and says nothing about what the rows mean, so `doctor`
also reports rows whose foreign key points at something that is gone, and tasks
stamped in the future — which sort ahead of real work and, on a claim, hold a
lease no sweep will ever retire. A green check that only ever proves the file
parses is a reassurance, not a diagnosis.

**An argument the command was never going to read is refused.** The
unknown-flag rule covered `--flags` and left positionals alone, so extra ones
were silently dropped: `kanban task add Fix the parser` recorded the title
`Fix`, and `kanban note t-1 the build is red --as ci` recorded the body `the`.
Both reported success. Forgetting to quote is the likeliest slip at a shell,
and this turned it into a durable record that was wrong with nothing to notice
it by — the ledger stating something untrue about work that was actually done.
Each row of `COMMANDS` now declares how many positionals its invocation may
hold, alongside its flags, so the table remains the single description of the
command surface and a new command cannot be added without saying. The error
prints the prefix it did accept — `after \`task add Fix\`` — because seeing
where the title stopped is what makes the cause obvious.

**Two requests at once is ambiguity, not a precedence puzzle.** `kanban claim
t-5 --next` named one task and asked for whichever comes first; it dropped the
id and handed back the head of the queue, so an agent that asked for `t-5` held
a lease on something else and had no hint of the swap. And a single-valued flag
given twice kept the last occurrence: `--project alpha --project beta` wrote to
beta — the wrong-board write ADR-007 exists to prevent, reached through a
repeated flag rather than a mistyped one, and trivially produced by a wrapper
script appending a default the caller had already set. Both are refused.
Last-wins is a common convention and the wrong one here, because the values
disagree, only one is what the caller meant, and nothing in the receipt says
which was used. Flags whose value is genuinely a list (`--depends-on`,
`--blocker`, `--validation`) are declared as such, and a compile-time guard
ties that list to the call sites that collect them so the two cannot drift.

**What declares the omission must survive the omission.** `truncated` was
computed honestly at the fetch layer and the rendering dropped content
independently of it. The compact form appended
`[context compacted: …]` and then trimmed the whole string to `--max-chars`,
so past the smallest budgets the marker was the first thing cut — the reader
got a packet missing the ancestry, the dependencies, every earlier checkpoint
and every note, ending mid-word, with nothing to say so. The marker is now
reserved out of the budget before the body is trimmed, as the full rendering
already did for `[older history omitted]`. A marker that a truncation can
truncate is not a marker.

**A flag that cannot apply on this path is refused, not ignored.**
`--max-chars` bounds the rendered text; `context --json --max-chars 8000`
accepted the flag and returned the whole packet, so a caller who asked for a
bounded packet got an unbounded one and no way to tell. The unknown-flag rule
exists because silent acceptance is undetectable by the caller; a known flag
silently doing nothing is the same defect wearing a legal name.

**A lease is granted on work, never on a container.** `claim` filtered on
status, dependencies, lane, assignee and `driver_only` — every property except
the one that says whether the row is executable at all. An epic and a story are
containers whose status is *derived*: `story advance` walks a story through its
gate and dispatches a separate task row for the actual work, and flips the
parent epic when the first story starts. Taking a lease asserts the opposite —
that one named agent is executing this row now — and writes `status='in_progress'`
and an assignee straight onto it. So a claimed story read `in_progress` on the
board while its own gate still read `planning`: the ledger stating two
contradictory things about one row, which is the failure this project exists to
prevent. It also parked a lease nobody could discharge, because a container is
never finished by working it, and hid the container from the queue for the
whole lease while not one of its children had moved. Worse, `claim --next`
sorts on priority alone, so an epic at the head of the queue was handed to the
first agent that asked for work.

Only a `task` is claimable. `--next` *skips* a container rather than failing on
it, because a row that was never claimable must not stall the queue behind it;
naming one explicitly is refused, and the refusal points at the gate
(`story advance`) or at the children, since an agent that is told only "no" has
no next move. The guard sits on both paths that mint a lease — `claim` and
`handoff accept` — rather than on the verb the operator happened to reach for:
eligibility is a property of the row. That second call site is not hypothetical
symmetry. A board written before this rule, or imported from atmux, can still
carry a pending handoff addressed to a container, and accepting it would mint
exactly the lease `claim` now refuses.

**A derived field is not writable by hand.** The same shape appeared one verb
over. A story carries two statements of where it is: `workflowStatus` in its
metadata, which the gate owns, and the `status` column every other reader uses.
The column is not independent data — `story advance` writes it on every step as
a projection of the gate (`planning`→`backlog`, `ready`→`todo`,
`in-progress`→`in_progress`, `done`→`done`, everything else →`review`). The
gate refuses an illegal transition by name, and then `task move` wrote the
projected column directly with no reference to the gate at all: a guard that
can be walked around is not a guard. `task move s-1 done` stamped `completed_at`
on a story that had taken no review signoff, dispatched no merge task, and
flipped no parent epic.

`task move` on a story is refused for the statuses the gate projects, naming
`story advance` and quoting where the gate actually stands, and `--force`
performs it and records `gateBypassed` — the same audited-override shape a
seized lease already uses. `blocked` and `cancelled` stay directly writable:
the gate is linear and can express neither, so guarding them would remove the
only way to say them rather than protect anything. The set of guarded statuses
is derived by running the gate's own states through the gate's own projection,
now one shared function rather than a literal repeated on each side, so a new
gate state cannot become hand-writable by being added in one place only.

Epics are deliberately untouched. An epic's status is moved by the gate only
once — the flip to `in-progress` when its first story starts — and no
`epic advance` verb exists, so `task move` is the intended mechanism for the
rest of its life. Refusing it there would strand the row rather than protect it.

**A hierarchy that is read is a hierarchy that is enforced.** All nine pairings
of parent and child type were accepted. Nothing in the ledger said an epic
contains stories and a story contains tasks — but three things read as if it
did: the id prefixes `e-`/`s-`/`t-`, the epic decomposition every consumer
performs, and `advance_story`, which flips a parent only when that parent is an
epic. So a story nested under a task was recorded, reported as success, and
then silently never flipped anything for the rest of its life. There is no
error to notice, because from the code's point of view the parent simply is not
an epic — which is also exactly what a correctly-nested story under a
mis-typed parent looks like.

A child must be a container its parent can hold: an epic contains epics,
stories and tasks; a story contains tasks; a task contains nothing.
(**Amended 2026-08-21** — this first shipped as "strictly narrower than its
parent", which refused epic-under-epic. A plan is an epic, so a programme plan
has to hold its sub-plans; the rule is stated as containment now rather than
computed from a depth, because depth arithmetic could only express that as an
exception bolted onto a rule it contradicts. Story-under-story, task-under-task
and container-under-something-narrower stay refused.) That is not a new model, it is the
one the boards already use — across the twelve live boards all 420 parent links
are epic→story, epic→task or story→task, and none of the six inverted shapes
occurs even once. Both writers of the field are guarded, `task add --parent` and
`task update --parent`, because re-parenting is the other way to say the same
thing. Recorded rows are left as they are, as with the priority band.

**Two requests at once, a fourth time: the board selectors.** The rule above —
naming one thing two ways is ambiguity, not a precedence puzzle — was applied to
`claim t-5 --next` and to a single-valued flag given twice, and left unapplied
to the three flags that select a board. `--db`, `--project` and `--workspace`
were read in that order, so `--project alpha --db /tmp/scratch.db` answered from
the scratch file and `--project alpha --workspace ../beta` wrote to alpha. This
is the wrong-board write [ADR-007](ADR-007-global-project-addressing.md) exists
to prevent, reached through two valid flags instead of a mistyped one, and
trivially produced by a wrapper appending a default the caller had already set.

The `--db` case is the sharper one, because opening a board creates it: a
`--db` path that did not exist was conjured empty and answered from, so a caller
who had also named a project got a board with nothing in it and an exit status
of zero.

At most one board selector may be given as a flag. The chain still resolves a
flag against its environment default and against the working directory, because
neither of those is a second request — `KANBAN_DB` and `KANBAN_PROJECT` remain
defaults a flag is free to override, and the working directory remains the
fallback when nothing else is said. Only flags the caller supplied are counted.
This supersedes the part of ADR-007's chain that let one explicit flag outrank
another.

**A bound that can ask for the opposite of a bound.** `--limit` was the one
numeric flag with no band: `--priority`, `--max-chars`, `--keep`,
`--lease-minutes` and `--stale-minutes` all validate, and this one went straight
into the query. SQLite reads `LIMIT -1` as *no limit*, so `attention list
--limit -1` and `events --limit -1` returned every row a caller had explicitly
asked to bound, and exited zero. It is the `--max-chars` defect exactly — the
caller asked for a bounded answer, got an unbounded one, and nothing in the
result says which they received — reached through a value the storage layer
reinterprets rather than a flag the command ignores.

Negative is refused; zero is not, because zero asks for nothing and returns
nothing, which is what it says, and a script computing a limit that comes out
zero is not making the mistake a negative one is. One reader owns the flag, and
a compile-time guard reads the shipping half of the file back and fails if any
call site goes around it — the same defense the numeric-parsing helper already
carries, for the same reason: this class of defect is introduced by a new call
site, not by changing the old one.

**An audit trail that records that something changed, and nothing about what.**
`task_updated` wrote an empty payload. For most fields that is merely thin — the
current value is on the row. For a body it is destructive: a plan is an epic's
body, and revising one replaced it with no record that the previous version had
ever existed. The trail is meant to be the thing you can reconstruct history
from, and here it recorded the fact of an edit as if that were the interesting
part.

The event now names every field that moved and carries the previous body when
the body moved. Only the body: everything else can be read off the row, and a
replaced body cannot. That makes `kb ev --task <epic>` the revision history of a
plan, which is history by construction rather than by anyone remembering to keep
a copy — the same reason attention items are resolved rather than deleted and
handoffs survive the task they were about.

**Two answers to one question, once more.** `--body` and `--body-file` both
supply the body, so giving both is refused rather than ranked, exactly as the
board selectors are. `--body-file` exists because a plan is markdown measured in
kilobytes, and putting that on a command line works and is miserable.

**A column that is always empty is not a record, it is a promise.** Checkpoints
and handoffs carried `repo_path`, `branch`, `head_sha` and `dirty_summary` from
the beginning, and filling them meant the caller passing `--repo --branch --head
--dirty` by hand. Measured 2026-08-21 across the twelve live boards, **0 of 20
checkpoints and 0 of 3 handoffs held a HEAD sha**. A resuming agent read
`branch: null` and guessed. The schema described provenance the ledger never had.

Provenance is now captured from the working directory rather than requested, and
an explicit flag still wins — capture is a default, not an override. A claim
records it too, which is what answers "which worktree is this lane in" on a box
running several lanes of one repository. A nested checkout also records the
outermost superproject's commit: a submodule's own sha says nothing about which
revision of the whole tree it belonged to, and the chain is followed to its end
rather than one level, because nesting is not limited to one level.

It asks git rather than reading `.git`. The layouts on a real machine disagree
with the tutorial — here a submodule has a real `.git` directory while its
superproject's is a file, lanes redirect through `gitdir:`, refs may be packed
and HEAD may be detached — and parsing that subtly wrong writes *wrong*
provenance into a durable record, which is worse than writing none. Every
failure degrades to absent: a command run outside a repository records no
provenance and is not an error.

**The trail could not say who created a task.** Every other event kind named its
actor; `task_added` recorded `None`, 132 times across the live boards, because
`task add` had no `--as` to record. It does now, and an absent actor is still
recorded as absent — inventing one would be worse than the gap.

**A migration that is never run is invisible.** `BOARD_V7` was written,
reviewed and compiled with the constant referenced nowhere: the build was clean,
the tests passed, and the first sign was a column missing at runtime on a board
that believed it was current. A guard now reads the file back and fails if any
declared migration is absent from the ladder that applies it. Its first two
versions were themselves wrong — one counted a text span that swept up the
test's own source, the next broke the moment `cargo fmt` wrapped the list — so
it checks by name, which no formatting can move.

**An empty answer is not a safe answer.** `task list --tag infr` could have
returned nothing and exited zero, and the caller would have read "no infra work"
rather than "that tag does not exist". A filter on an unregistered value is now
refused and names the nearest registered one, because an empty list is the one
wrong answer that looks exactly like a right one. The same reasoning already
applies to a mistyped flag and a mistyped board; a mistyped filter had been
missed because it fails silently rather than loudly.

**A clearing flag needs the flag it clears declared beside it.**
`--tag infra --clear-tags` was accepted and silently preferred one of them —
two answers to one question, decided by whichever branch the dispatch tested
first. The `clear-` family is now paired in the mutually-exclusive list, and a
guard reads the file back so the next `--clear-x` cannot ship without its pair.
`--no-cross-lane` is deliberately exempt: there is no `--cross-lane` to
contradict, so a pair would be a flag nobody needs.

**A closed pipe was reported as a failure.** `kb task list --json | head`
printed a Rust panic and a backtrace note over the output it had just produced,
then exited non-zero, because `println!` unwraps its write. Every other Unix
tool ends quietly when its reader leaves, and a pipeline could not tell "head
had enough" from "the command failed". Output now goes through one writer whose
`BrokenPipe` is recognised at the exit point and treated as a normal ending —
scoped to that one error kind, so a real failure still exits 1 and still says
why.

The test for it was wrong before it was right, in a way worth recording: driven
through a shell pipeline, it passed with the fix removed, because **a
pipeline's exit status is the last command's** and `| head` exits 0 whatever
happened upstream. It closes the reader directly now. The general form is the
one this project keeps hitting — a test that exercises the right behaviour
through a mechanism that cannot observe it is a test that reports success for
the wrong reason.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) — durable resume contract
- [ADR-003](ADR-003-private-multi-project-personal-work-system.md) — private storage
- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — compiled-binary E2E
- [ADR-007](ADR-007-global-project-addressing.md) — board selection chain
- [ADR-015](ADR-015-tags-are-a-per-board-master-file.md) — the tag registry, whose refusals are these rules applied to a vocabulary
