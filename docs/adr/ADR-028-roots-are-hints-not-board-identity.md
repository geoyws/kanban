# ADR-028: Roots are hints, not board identity

**Status:** Accepted
**Date:** 2026-08-27
**Deciders:** George
**Amends:** ADR-003 project registration and ADR-007 board addressing

## Context

[ADR-003](ADR-003-private-multi-project-personal-work-system.md) made storage
global and [ADR-007](ADR-007-global-project-addressing.md) made addressing
global: a board lives under the operator's private data directory, and
`--project NAME` or `KANBAN_PROJECT` reaches it from any directory on the host.
Identity stayed local. The registry's `workspaces` table is keyed by
`root_path` and holds `board_path` as a UNIQUE column, so every board has
exactly one privileged root, that root's row is where the board's name is
stored, and a board with no root at all cannot be represented.

Extra roots are not new. `workspace_aliases` attaches further worktrees to the
same board, and `workspace list` already reports them as `canonical: false`
beside one `canonical: true` row — `px` is canonical at
`/root/work/ifca/src/crm-react` and non-canonical at
`/root/work/ifca/src/pai-root`; `kanban` carries a driver-worktree alias. The
many-to-one shape exists. What fails is the privileged head that shape hangs
from.

**A repository legitimately exists at several paths at once.** The `atmux`
board's canonical root is `/root/work/src/atmux`, and the identical repository
is also a submodule at `/root/work/geoyws/src/root/projects/atmux`. Neither
path is wrong and neither is a copy. Resolution walks up from the working
directory to the nearest registered root, so an agent working in the submodule
checkout does not fail to find a board — it silently finds the *enclosing*
`root` board and writes there. `init` in the submodule refuses as a nested
registration and steers the operator toward attaching to that enclosing
project, which is the wrong board again. Only an explicit
`workspace attach --to /root/work/src/atmux` produces the right answer, and
nothing prompts for it.

**The path names the board.** `kanban init` in `/root/work/geoyws/src/root` on
2026-08-27 created a board named `root`, because the name falls back to the
directory basename when `--name` is absent. The intended name was `geoyws`.
Under ADR-007 that name is the authoritative address every agent types, so the
directory layout decided the public identity of the board, and decided it
wrong. There is no rename operation in the command surface; the only repair is
re-running `init` at the canonical root.

The same coupling lets the name drift. An alias row carries a copy of the name
taken at attach time, and registration updates whichever table the given root
sits in, so `kanban init --name X` at an *alias* root renames that alias and
the board file's own `board_meta` while `workspaces.name` — the name
`--project` resolves and `ONLY:<board>` rule selectors match — stays as it was.
Path-based board-name selection returns the alias copy, so after such a drift
an agent standing in that worktree is served a different rule set than one
standing in the canonical root of the same board.

**A deleted root strands its board.** `workspace detach` refuses a canonical
root outright, and `workspace repoint` refuses a root that no longer exists
anywhere because there is nothing to repoint it to. A board whose canonical
directory was deliberately deleted therefore keeps an unreachable root forever:
`doctor` reports the whole registry unhealthy on every run for as long as that
root stands, and nothing can retire it without destroying the registration that
carries the board's name. Draft task `t-73b02124` on the
`kanban` board records exactly this. The board itself is perfectly healthy and
still reachable by name — the code already documents that an unreachable root
leaves a project "reachable only by name" — which is the evidence that name
addressing carries a board fine and the root was never the load-bearing part.

## Decision

**The board name is the identity.** `--project NAME` and `KANBAN_PROJECT` are
the authoritative address, as ADR-007 already made them. A board's name belongs
to the board, not to any directory, and no filesystem operation changes it.

**A root is a convenience pointer.** Its only job is to let an agent standing
in a directory be *guided* to the board it probably means. It is a hint,
resolved by the nearest-enclosing-root convention that already exists, and it
never carries identity.

From those two:

- A board is creatable and fully usable with **zero** roots. Registering a
  board and pointing a root at one are separate operations, and the second is
  optional.
- Roots are an unordered many-to-one set attached to a board. No member is
  privileged; there is no canonical root.
- Adding, moving, retiring or losing every root leaves the board's name,
  storage and contents untouched. Root maintenance is never destructive to work
  state, and losing the last root degrades discovery only.
- A root may be retired at any time, including the last one, without inventing
  a replacement path.
- Board naming is explicit. `init` requires a name rather than inferring one
  from a basename, and a board is renamed by an operation that names the board,
  not by re-registering a path.
- One board holds exactly one name. The name lives in one registry row; the
  copy inside the board file remains a self-describing fallback for a board
  found without its registry, and a rename writes both.

Resolution and refusals keep their existing shape. The chain in ADR-007 is
unchanged in order and meaning; only rule 4's subject changes from "the
canonical root that contains the working directory" to "a root that contains
it". Registry names remain non-unique, so an ambiguous `--project` stays an
error naming its candidates rather than a pick — per
[ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md).
Because a candidate may now have no root to name, that error lists every root
each candidate has, says plainly when a candidate has none, and keeps
`--workspace` as the disambiguator for those that do. A rootless duplicate name
is addressable only after a rename, which is the second reason renaming has to
exist rather than being a convenience.

Explicitly not decided here: making board names unique. That would resolve
duplicates by construction, but it is a separate change with its own migration
over live boards, and ADR-007's diagnosable-ambiguity behaviour is adequate in
the meantime.

## Implementation status

**No shipped code behaves this way yet.** This ADR records the decision and its
rationale; the current binary still keys board identity to a canonical path
everywhere below. An implementation would have to change at least:

- `rust/db.rs:791-802` — `REGISTRY_V1`/`V2`: `workspaces.root_path` as PRIMARY
  KEY with `board_path` UNIQUE, and `workspace_aliases` as the second-class
  table. This is where a board with no root becomes unrepresentable.
- `rust/registry.rs:198-249` — `Registry::register`: creates the board and its
  privileged root in one insert, and renames whichever table the root sits in.
- `rust/registry.rs:265-323` — `Registry::exact` / `resolve` /
  `resolve_readonly`: two-table lookup that prefers `workspaces`, and the
  upward walk that makes an enclosing board absorb a nested checkout.
- `rust/registry.rs:325-354` — `Registry::attach`: copies the canonical name
  into the alias row, which is the drift described above.
- `rust/registry.rs:388-450` — `Registry::detach`: refuses a canonical root.
- `rust/registry.rs:1132-1173` — `Registry::projects` / `by_name`: builds every
  `ProjectRecord` from the `workspaces` table, so a board without a canonical
  row would be invisible to `--project`.
- `rust/registry.rs:1210-1275` — `unreachable_roots` / `repoint`: reports a
  deleted root forever and refuses to repoint one that resolves nowhere.
- `rust/lib.rs:1024-1043` and `rust/lib.rs:1140-1230` — `board_by_name`,
  `store_path`, `store_path_readonly`: the ADR-007 chain, plus the duplicate-name
  error that names `canonical_root`.
- `rust/lib.rs:1397-1423` — `selected_board_name`: returns the per-root name
  copy, so rule scoping follows the drifted alias.
- `rust/lib.rs:1690-1705` — the `init` handler: the basename fallback that
  named a board `root`.
- `rust/lib.rs:1828-1829` — `doctor`: a single unreachable root makes the whole
  registry report unhealthy, with no way to clear a deliberately deleted one.
- `rust/model.rs:351-371` — `WorkspaceRecord.canonical` and
  `ProjectRecord.canonical_root`: the public JSON that encodes the privilege.
- `rust/serve.rs:990-998` — the web board header renders `canonical_root`.
- `docs/PRD.md:39`, `README.md:255-275`, and
  `docs/testing/compiled-rust-e2e-matrix.md:10` document the canonical-root
  model and must move with it.
- `tests/e2e.rs` around lines 6820-7025 assert the current semantics,
  including that a canonical root cannot be detached.

## Migration and compatibility

A registry schema revision splits identity from location: one row per board
carrying its name and `board_path`, and a separate roots table whose rows
reference a board. Every existing `workspaces` row becomes one board row plus
one root row; every `workspace_aliases` row becomes one further root row for
the same board. The board's name is taken from the former canonical row,
because that is the name `--project` and `ONLY:<board>` rule selectors already
resolve today.

Per-root name copies are dropped rather than merged. Where an alias name
disagrees with the board name — the drift described above — the migration
records the discarded value in a registry event rather than discarding it
silently, so a board whose rules were being scoped by a stale alias name is
diagnosable afterwards instead of merely fixed.

`workspace_alias_history` continues to hold retired roots, and former canonical
roots become eligible to join it. A board whose only root is unreachable
migrates to a board with one retirable root; retiring it leaves a rootless
board that `doctor` reports as rootless rather than as broken, and that remains
fully usable by name. This is what settles `t-73b02124`.

`canonical` and `canonical_root` are removed from the public records rather
than retained with a fabricated value. After this decision no root is
privileged, so any value the fields could carry would be a claim that is not
true, and a consumer filtering on `canonical == true` would silently receive an
empty set. Removing the fields makes such a consumer fail visibly, which is
ADR-008's rule applied to a JSON contract. Known readers inside this repository
are listed above; external consumers of `workspace list` and `dashboard` must
be audited and updated in the same delivery.

`init` requiring an explicit name breaks any caller that relied on the basename
fallback. That break is deliberate and loud: the alternative is more boards
named after whatever directory an agent happened to be standing in. `init`
keeps its existing refusal for a path already covered by a registered root, so
nested registration still has to be asked for.

Boards, board files, backups and restores are untouched by this migration. No
task, event, lease, handoff or rule row moves, and the on-disk board format
does not change.

## Consequences

One repository checked out at several paths — standalone and as a submodule of
another repository — points every checkout at the same board without one of
them being the real one. The submodule case stops silently writing to the
enclosing board once a root is attached, and attaching a root becomes an
ordinary, reversible act rather than a permanent structural commitment.

Deleting a directory becomes an ordinary event. A board outlives the tree it
was created next to, which is the correct relationship: the work ledger is not
a property of a checkout, and a checkout is frequently disposable.

Board names become worth choosing, because nothing else supplies one. That is
the cost of the decision and it is intended: an explicit name is a decision
made once by the operator, where the basename fallback was a decision made
implicitly by a directory and discovered later.

Discovery weakens for a rootless board. An agent standing anywhere is guided to
nothing and must address the board by name. That is the accepted trade, and it
is bounded by the fact that name addressing already works from anywhere; a
rootless board is less convenient, never less usable.

The registry gains a real board entity, which future work — rename, per-board
metadata, board-level retention — attaches to instead of to a path. Nothing
here implements any of that.

## References

- [ADR-003](ADR-003-private-multi-project-personal-work-system.md)
- [ADR-007](ADR-007-global-project-addressing.md)
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
- [ADR-027](ADR-027-rules-are-one-tag-scoped-kb-document.md)
- [Product requirements](../PRD.md)
