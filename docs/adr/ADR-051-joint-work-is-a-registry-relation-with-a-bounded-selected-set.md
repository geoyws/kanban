# ADR-051: Joint work is a registry relation with a bounded selected set and role-typed evidence

**Status:** Proposed
**Date:** 2026-09-26
**Deciders:** the `t-fe137b57` writer, under the bounded scope George approved on 2026-09-25
(resolved attention `a-93efef34`: epic `e-73bf760f` + spec task `t-fe137b57` to `todo`, sibling
tasks stay `draft`). George may supersede any clause of it.
**Supersedes:** nothing. [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md) (plans
stay epics), [ADR-041](ADR-041-transact-is-one-atomic-ordered-write-batch.md) (single-board
atomic batches) stay in force unchanged for their own slices. This decision takes no
implementation dependency on `e-df626704` (`todo`) — see §3 — and adds one relation beside them.

**Scope correction (2026-09-30):** George withdrew `LINKED-23` under
`a-53b18f9a` after ADR-053 retired the served web UI. Its ID remains
reserved in `docs/specs/linked.md`; the original wording stays there for
history. This proposed ADR describes the registry relation, bounded work
set and delivery evidence only. It proposes no replacement UI requirement
and grants no implementation or release authority.

## Context

Epic `e-73bf760f` (status `todo`) asks for joint Unum/Acies features with three properties that
do not exist anywhere in the tree: two boards expose the same relationship, the same
attribution of who worked each task, and the same delivery evidence of which repo commits
delivered it — while each board keeps owning its own epics and tasks, and while a worker bound
to the joint feature may claim only explicitly selected tasks.

Three shapes were available for the relation:

- **Mirrored rows on both boards.** Each board stores its own half of the pairing and the two
  halves are kept in step. Rejected: two rows kept in step is two chances to disagree, and every
  rename, retirement, and recreation becomes a distributed update with a window in which one
  side shows a companion the other denies — exactly the split-brain the epic's acceptance A
  forbids.
- **A merged board (or copied tasks).** Fuse the two boards, or copy the joint tasks onto one
  of them. Rejected: it destroys the ownership the epic requires each board to keep, and it
  duplicates rows whose states and leases would then diverge.
- **One stored relation in the authoritative registry, exposed from either side.** The pairing,
  the selected set, the bindings, and the contributions live in exactly one place — the registry
  on `hax` — and both boards read the same row. There is no second copy to drift.

Three shapes were likewise available for the selected work set:

- **An epic holding the selected tasks.** Rejected: a plan is an epic and its children are the
  work it became ([ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md)); making the
  scope an epic would turn scope membership into parentage, silently adopting every child
  (LINKED-10 forbids exactly that) and tangling joint planning with joint scoping.
- **A sprint holding the selected tasks.** Rejected: a sprint is a typed version boundary that
  closes on a served version
  ([ADR-045](ADR-045-sprints-are-proof-gated-version-boundaries.md)); scope is not a version and
  must outlive any number of them.
- **Its own registry record with audited revisions.** The set is a first-class row with a
  revision chain, independent of epics and sprints. This is the decision.

And two shapes for the cross-board write:

- **Stretch `transact` across registries.** Rejected:
  [ADR-041](ADR-041-transact-is-one-atomic-ordered-write-batch.md) made `transact` one atomic
  ordered batch on one board; spanning two failure domains with one atomicity claim is a promise
  the store cannot keep, and a silent rollback of a landed first half would destroy evidence.
- **An ordered pair of single-board writes with a reported, compensatable second half.**
  Durable and fail-closed, never atomic. This is the decision.

## Decision

Joint work is a registry relation with a bounded selected set and role-typed evidence, in four
parts:

1. **One stored relation, exposed from either side.** A companion pairing between one Unum item
   and one Acies item is stored exactly once in the authoritative registry. Endpoints are
   `(boardID, id)` with stable board UUIDs and exact item IDs, each pinned to its incarnation;
   display names, board paths, and `@#` tokens are never persisted identity and never write-time
   identity. Symmetry is a property of the one row: retiring from either side retires both
   exposures in the same change.

2. **The selected set is its own registry record, not an epic and not a sprint.** It carries
   audited append-only revisions (number, author, reason, exact member list) on the hash-chained
   journals. The selected-scope gate is one gate across candidates, `--next`, named claims,
   lease-taking handoffs, and resumption, added beside — never instead of — every existing gate.
   The binding names one `(actor, lane, session)` triple with exactly three endings (release,
   expiry, authorized exit/rebind) — revocation is the authorized exit, and the old triple is
   rebindable only by a new audited revision. Frozen parks taking while holding runs on, and
   freezing reverses only through an audited UNFREEZE; revoked ends taking while heartbeats run
   to expiry or release, and a revoked lease ends only via `release`, never by handoff onward.

3. **The ordered cross-registry pair, never an atomic span.** A joint change is two single-board
   writes in order; the second is attempted only if the first landed. Membership and lease
   revisions carry write-time revision checks. `transact` naming two boards is refused whole.
   Each half is authorized as if it arrived alone under its store's existing authorization, and
   serialization is the store's single-writer transaction plus this decision's write-time revision checks — no cross-board primitive is used, so none of the sibling's is redefined: companion never satisfies a readiness gate and never appears in a `blockingGates` answer.

4. **Role-typed evidence with a non-code disposition.** Contributions are append-only records
   with repo identity, full object IDs, actor/lane/session, task, and evidence. Each declares
   exactly one of four roles — starting HEAD, implementation commit, accepted integration
   commit, tested consumer candidate — with the squash mapping retained and the consumer's exact
   commit plus complete nested dependency path required. A moving `latest` and every near-miss
   (wrong hash, stale pin, wrong path, changed candidate) satisfy nothing. Non-code deliverables
   use kind `non-code` with the same identity discipline minus hash and role, and the kinds
   never satisfy each other.

The cross-board field spelling is `(boardID, id)` everywhere — flags, JSON, and events — the
spelling commissioned for both slices in the approved scope (George, 2026-09-25, `a-93efef34`),
defined here from that source. This decision takes no implementation dependency on the sibling
and redefines nothing of its resolver, authorization, or locks.

## Consequences

**What becomes harder.** Any joint-feature change now costs a pairing row, a membership
revision, or a contribution record before it counts, and the three documents (this ADR, the
LINKED specification, the matrix rows) move in one change when their meaning moves together. A
worker that could previously claim anything on its board can now claim only its selected set on
every path, including handoff and resumption — which is the point, and which will refuse first
and teach second.

**What becomes possible.** Two boards describe one feature without merging; either side reads
the identical relationship, attribution, and evidence after any restart; revocation and freeze
are audited operations rather than conversations; and joint closure is a check over exact
evidence rather than a judgement call.

**What is deliberately not decided.** The CLI verb names and flag spellings beyond the
`(boardID, id)` field shape — those are implementation surface under this decision, and the
specification's §5 fixes only the fields. No performance, availability, or retention commitment:
timing is proven by the same-change real E2E plus the release gate in Linux Docker, measured at
release.

## References

- Epic `e-73bf760f` (`todo`) — the authoritative requirement source; task `t-fe137b57` commissioned this ADR and the LINKED specification
- George, 2026-09-25 — bounded-scope approval in resolved attention `a-93efef34`
- `e-df626704` (`todo`, cross-board readiness gates) — the COMPLEMENTED sibling: companion != depends-on, scope membership != either; shares only the commissioned `(boardID, id)` spelling, with authorization and serialization defined here
- `docs/specs/linked.md` — the requirement-level statement of this decision (`LINKED-01`..`LINKED-25`)
- [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md) — why the selected set is not an epic
- [ADR-041](ADR-041-transact-is-one-atomic-ordered-write-batch.md) — why the pair is ordered, not atomic
- [ADR-045](ADR-045-sprints-are-proof-gated-version-boundaries.md) — why the selected set is not a sprint
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — why the refusals name their fix
- [ADR-029](ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md) — the chain the revisions join
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — why CLI and MCP agree (one operation, one tool)
- [ADR-047](ADR-047-kanban-adopts-specification-driven-development.md) — the conventions the specification is written under
