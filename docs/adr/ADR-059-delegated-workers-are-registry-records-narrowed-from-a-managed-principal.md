# ADR-059: Delegated workers are registry records narrowed from a managed principal

**Status:** Proposed
**Date:** 2026-10-01
**Deciders:** George
**Sources:** kb kanban `a-eaa4d835` (IDENTITY slice admitted for `t-2aafd55c` only, 2026-09-30),
`a-75f7280c` and `a-ed2bbd7d` (owner verdicts, 2026-10-01); row `t-2aafd55c`; epic
`e-3a942ae9` (approved by George 2026-09-21).

## Context

Agent lanes delegate work to subagents. Today a subagent that touches the ledger is
indistinguishable from its lane: `--as`, `--lane`, `--session` and `--to` are free-text strings
checked only for shape (`rust/store.rs:18-24`, `:125-127`). The one unforgeable element on the
claim path is the lease token, an exact-match bearer secret (`rust/store.rs:3269-3291`), which is
bound to no process, no principal and no attempt. Under managed enforcement the kernel peer UID
does mint a sealed principal (ADR-033, ADR-038, `rust/broker.rs:441-606`,
`rust/routing.rs:259-291`), and that principal's grants are the closed lattice
`read < write < admin` over the scope tuples `registry`, `board:B`, `board:B + tag:T` and
`board:B + *` (`rust/policy.rs:20-122`). But nothing narrower than a principal exists, the
principal is never stamped on claim, checkpoint or handoff rows, there is no claim attempt
counter, and no claim-path request is idempotent (`rust/mcp.rs:762-764`).

All of one operator's agents run under one UID. The kernel therefore cannot tell one of them
from another. Any narrower identity has to be something the operator's principal hands out and
the ledger checks, not something the kernel proves.

Work is specified in `docs/specs/identity.md` (slice `IDENTITY`, `IDENT-01` .. `IDENT-18`).

## Decision

1. **Managed-only** (`IDENT-01`, `IDENT-18`; owner verdict `a-75f7280c`). Worker identity exists
   only where the registry's enforcement state is `managed`. Under `direct` or `prepared` every
   worker verb and every worker credential is refused, and the free-text `--as` path keeps
   working exactly as today with no anti-spoofing claim made for it.
2. **A worker is a registry record, owned by a principal** (`IDENT-02`, `IDENT-03`). The
   registering process's own managed principal (never a request field) owns it. Kanban mints the
   worker id; the run id, harness agent id and lane actor are labels the registrant supplies, kept
   immutable for accountability and never consulted as authority. Registration, retirement and
   the grant set are policy events, so they join the hash-chained journal and advance the epoch
   like every other policy change (ADR-029, ADR-038 clause 4).
3. **Grants come from the closed vocabulary and only narrow** (`IDENT-04`, `IDENT-06`; owner
   verdict `a-ed2bbd7d`). A worker's grant is a set of `(scope tuple, capability)` pairs in the
   existing vocabulary. Each pair must be satisfied by the registrant's effective authority when
   registered. A worker's effective authority is recomputed on every call as its own grant
   capped by its parent's effective authority, back to the principal's live grants, so revoking a
   principal grant narrows every descendant at once. An optional task root restricts a worker to
   one task subtree; a child's root lies inside its parent's.
4. **A worker proves itself with a minted credential, bound to its principal** (`IDENT-05`). The
   credential is shown once at registration, stored only as a SHA-256 digest, and presented in
   the environment variable `KANBAN_WORKER_CREDENTIAL`, never on a command line. A credential
   presented by any other principal is refused with the generic denial.
5. **Leases are worker-bound and counted** (`IDENT-08` .. `IDENT-10`). A worker's claim records
   its worker id and principal on the lease, requires write over the task's tags, its task root
   and its own lane actor, and receives the task's next attempt number. Every lease-bound call by
   a worker rechecks its effective authority. The exact-token fencing and its refusals stay as
   they are.
6. **A closed list of worker operations** (`IDENT-11`, `IDENT-12`). A worker may read, claim,
   heartbeat, release, checkpoint, note and hand off its own leased work, and manage its own
   descendants. Everything else is coordinator-only and refused by name. A worker without `write`
   holds no lease.
7. **No lane-global or parent-global lock** (`IDENT-13`). Coordination stays one lease per task.
   Workers of one lane and siblings of one parent claim different tasks concurrently.
8. **Idempotent claim-path requests** (`IDENT-14`). `claim`, `checkpoint` and `handoff create`
   accept `--request-id`. A replay with the same request returns the stored response and writes
   nothing; a different request under a used key is refused. This works in every enforcement
   state because it asserts no identity.
9. **Versioned migration, legacy rows unchanged** (`IDENT-17`). Registry schema 15 adds the worker
   tables; board schema 38 adds the attempt counter, the principal and worker columns and the
   request receipts. Existing leases keep principal-level behaviour until they end. An older
   binary refuses the newer schema with its existing sentence.

## Alternatives rejected

- **Enforce worker identity in direct mode too.** Rejected by George on `a-75f7280c`: direct mode
  has no authenticated caller, so a worker check there would be a free-text claim dressed as
  authority.
- **Grants derived from the lane actor or a new capability vocabulary.** Rejected by George on
  `a-ed2bbd7d`: a worker grant comes from the existing closed vocabulary and only narrows.
- **A per-worker Unix account.** It would let the kernel tell workers apart, but it needs a host
  account per subagent, which the single-operator ledger rules out (epic `e-3a942ae9`: no
  multi-user account system).
- **The lease token as the worker identity.** A token is minted per claim and dies with it; a
  worker needs one identity across many leases, and a parent needs to name its children before
  they claim anything.
- **A lane-wide or parent-wide busy flag.** Epic `e-3a942ae9` forbids lane-global and
  parent-global serialization; one lease per task already gives one writer per task.
- **Credentials on the command line.** Process listings expose arguments to every local user.

## Consequences

- Delegation narrows authority but does not defend a principal against itself. Any process of
  the same UID may act as the principal directly, as it can today; the worker layer stops a
  credential from being used beyond its grant, task root and operation list, and stops another
  principal from using it at all.
- Rollout needs the global agent policy (dotfiles `AGENTS.md`, delegation bullet) to record an
  explicit exception that lets a delegated subagent hold a worker credential for exactly the
  operations in `IDENT-11`. Until that exception is recorded, implementation may land but no lane
  may hand a credential to a subagent.
- Each registration and retirement advances the policy epoch, so an in-flight context minted
  before it is refused at commit and retried (ADR-038 clause 8).
- Request receipts are kept as long as the board; a replay of a claim returns the original lease
  token even after that lease has ended, and the next lease-bound call then gets the normal
  refusal.
- Implementation is row `t-2aafd55c`, which writes the tests named in the slice's trace.
