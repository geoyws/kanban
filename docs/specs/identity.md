# Specification: under managed enforcement a principal registers delegated workers whose authority only narrows, whose leases are counted, fenced and stamped, and whose claim-path requests are idempotent, while direct mode runs unchanged (slice IDENTITY)

## 1. Identity and baseline

- **Slice ID:** `IDENTITY`. Requirement IDs are `IDENT-01` .. `IDENT-18`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-10-01` at commit `60e5327` (`origin/kanban-geoyws-driver`) in
  `/Users/geoyws/work/wt/kanban-t-026ece7c-identity-w2-e74575`, branch `wt/t-026ece7c-identity`.
  Every "today" claim below cites `<path>:<line>` in that worktree. At the baseline no worker,
  run, delegation, attempt or request-receipt concept exists in `rust/`; `tasks.parent_id`
  (`rust/db.rs:16-19`) is work breakdown, not authority.
- **Status:** `SPEC-READY` on 2026-10-01 (independent reviewer agent `ReviewIdentitySpec1` at
  commit `5d2f1a7`, applying the SDD §1 exit criteria: eighteen MUST requirements, each testable,
  single-layer and sourced, reached by an acceptance example and one §8 row identical to the
  matrix; both material owner choices taken on `a-75f7280c` and `a-ed2bbd7d`; cited lines
  spot-checked in this worktree. Its six wording findings are applied as recorded in §9.
  Specification readiness only — it authorises neither implementation, nor rollout, nor release.)
- **Owner (product scope):** George. He alone resolves scope and the open questions in §7.
- **Decider (wording of this document):** slice row `t-026ece7c` under epic `e-c0852fe7`,
  admitted on owner verdict `a-eaa4d835` (resolved by geoyws 2026-09-30: "IDENTITY slice approved
  for t-2aafd55c ONLY. It does not absorb claim routing ... docs/specs/identity.md plus ADR,
  independent review, stop at SPEC-READY").
- **Sources:**
  - Owner verdict `a-75f7280c` (2026-10-01): "Managed-only delegation. New worker capabilities are
    enforced only under broker peer identity; direct-mode legacy claims remain compatible but
    cannot impersonate managed delegation." Note: "the spec claims no anti-spoofing there."
  - Owner verdict `a-ed2bbd7d` (2026-10-01): "Broker registry grants. Use existing closed managed
    capability vocabulary and intersect worker grant with parent and task scopes; requires
    explicit policy mapping and migration for lease-bound scope."
  - Implementing row `t-2aafd55c`: "canonical lane actor plus immutable worker/run/parent
    identity; registration authority and spoofing refusal; task attempt/generation; narrow
    capability scope and ancestor scope intersection; exact-token fencing; read-only assignments
    versus writer ownership; bounded output schemas; idempotent requests; versioned migration";
    "without imposing lane-global or parent-global serialization"; "raw SSH, arbitrary ledger
    mutations, external writes and Git/deploy remain coordinator-only".
  - Epic `e-3a942ae9` (approved by George 2026-09-21): "Worker fields supplement rather than
    replace lane/actor"; "Nested delegation cannot widen ancestor scope"; "Same-lane workers must
    remain distinct. Idempotent retries return the same assignment and do not mint duplicate
    leases"; "Coordination is many independent task leases, not one lane-global busy flag";
    "Read-only workers get identity and assignments but no unnecessary write lease"; "no new
    ledger, host migration, multi-user account system".
  - `docs/adr/ADR-059-delegated-workers-are-registry-records-narrowed-from-a-managed-principal.md`
    — the decision this slice implements.
  - `docs/adr/ADR-033-principals-are-frozen-username-plus-uid-and-minted-through-a-peer-credential-broker.md`
    and `docs/adr/ADR-038-linux-principal-broker-policy-and-bootstrap.md` — principals, the closed
    lattice, events-are-truth, the epoch check.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — every refusal
    names what was refused and writes nothing.
  - `docs/adr/ADR-010-adapters-generated-from-the-command-surface.md` and
    `docs/adr/ADR-011-in-binary-mcp-server-and-in-place-reload.md` — CLI, schema manifest and MCP
    tools come from one command table.
  - `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` — policy events
    and access-audit rows, denied attempts included, are hash-chained journals; IDENT-03 adds
    entries to them and no new journal.
  - Shipped surface at the baseline:
    - Direct identity is free text: `--as`, `--lane`, `--session` are trimmed and checked
      non-empty only (`rust/store.rs:18-24`, `:125-127`); the lane grammar is
      `driver_lane_name`, `typed_driver_lane`, `claim_lane`, `same_claim_worker`
      (`rust/store.rs:45-123`, claim-routing `CLAIM-01` .. `CLAIM-03`).
    - The lease: `task_claims(task_id PRIMARY KEY, agent_id, session_id, lease_token UNIQUE,
      claimed_at, heartbeat_at, expires_at, ...)` (`rust/db.rs:241-261`); token minted as a v4
      UUID at claim and at handoff accept; `require_lease` matches task, token and unexpired
      exactly and refuses with the superseded or never-held sentences without naming the token
      (`rust/store.rs:3266-3291`); the holder check "lease belongs to X, not Y"
      (`rust/store.rs:7819-7821`, `:9517-9519`). No attempt counter, no principal column.
    - No idempotency key on claim, checkpoint, handoff or sitrep; `transact` says so in its MCP
      description (`rust/mcp.rs:762-764`). Reusable precedents: `deployments.operation_id` UNIQUE
      with an idempotent replay (`rust/db.rs:815`, `rust/store.rs:10603-10657`) and dispatcher
      delivery attempts (`rust/model.rs:199-221`).
    - The managed estate: the sealed `PrincipalContext` minted from kernel peer credentials, the
      passwd two-way check and the registry's frozen principal (`rust/broker.rs:441-606`); the
      in-process equivalent `local_authority`, which refuses root and reads the canonical
      registry, never `KANBAN_DATA_DIR` (`rust/routing.rs:259-291`); `board_authz` supplies an
      empty authority outside `managed` (`rust/routing.rs:234-257`). Enforcement states are
      `direct`, `prepared`, `managed`, and an unknown state reads as `managed`
      (`rust/routing.rs:94-113`).
    - The closed vocabulary: capabilities `read < write < admin` (`rust/policy.rs:20-51`); scope
      tuples `registry`, `board:B`, `board:B + tag:T`, `board:B + *` (`rust/policy.rs:53-122`);
      the pointwise join `authority` and the wildcard-satisfier rule `satisfies`
      (`rust/policy.rs:534-574`); the all-of-tag row checks `check_read` and `check_write`
      (old and resulting tag sets), refusing with the single string `denied or not found`
      (`rust/authz.rs:34`, `:66-145`). Policy changes are chained `policy_events`
      (`rust/policy.rs:850-851`) that advance the epoch.
    - Schema: board version 37, registry version 14 (`rust/db.rs:2945-2946`); a newer database
      is refused with "database version N is newer than supported version M"
      (`rust/db.rs:3383-3390`).
    - MCP tools are generated from the command table and carry `readOnlyHint`
      (`rust/mcp.rs:221-330`, `:316`); each tool call spawns the binary, so the tool's caller
      identity is the child process's own (`rust/mcp.rs:404-431`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`, section
  `## Requirements trace — docs/specs/identity.md IDENT-01..IDENT-18`. §8 is its draft.

## 2. Purpose and scope

**Intended outcome.** On a managed installation, a lane can hand a subagent a credential that
lets it do exactly the claim-path work it was delegated — on the tasks it was delegated, with no
more authority than the lane holds — and the ledger records which worker, of which run, under
which parent, held each lease and on which attempt. A retried request does not mint a second
lease. A direct installation behaves exactly as at the baseline.

**Users / actors.**

- The coordinator: a lane's main loop, running as the operator's managed principal. It registers
  workers, hands out credentials, and keeps every coordinator-only operation.
- A delegated worker: a subagent process holding one worker credential. It may register its own
  child workers.
- The operator (George), who owns the principal grants from which all worker authority derives.

**In scope.** Worker records and their verbs (`worker register`, `worker show`, `worker list`,
`worker retire`); the worker credential; effective worker authority; worker-bound claims and
their stamping; the claim attempt counter; the closed worker operation list; request ids on
`claim`, `checkpoint` and `handoff create`; the matching MCP tools; the migration from registry
schema 14 to 15 and board schema 37 to 38.

**Boundaries.** Principals, grants, enforcement transitions, break-glass and the broker are
unchanged (ADR-033, ADR-038). The lease token's minting, fencing and refusal sentences are
unchanged (`rust/store.rs:3266-3291`). Claim routing (`docs/specs/claim-routing.md`) is unchanged.
Git, deploys, worktree creation and external messages are outside Kanban and are governed by the
agent policy, not by this slice.

**Non-goals.**

- Anti-spoofing in `direct` or `prepared` mode. Owner verdict `a-75f7280c`: "the spec claims no
  anti-spoofing there."
- Distinguishing processes of one principal from each other by the kernel. All of one operator's
  agents share a UID; the worker layer narrows what a credential can do, it does not stop the
  principal from acting as itself (ADR-059 Consequences).
- Group or work-group reservations. The epic admits them only "if the consumer needs it"; no
  consumer has asked.
- Durable worktree and e2e provenance records (epic `e-3a942ae9`, a separate implementation task).
- A new capability or scope vocabulary (owner verdict `a-ed2bbd7d`).
- Recording the agent-policy exception that lets a subagent hold a credential. That is a dotfiles
  change and a rollout precondition (ADR-059 Consequences), not a Kanban requirement.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser. Managed cases
  run under a non-root UID against a managed registry, as `tests/authz_bypass_matrix_e2e.rs`
  already does.

This slice changes no served markup, so no requirement uses `http` or `chrome`.

Terms. *Managed* means the canonical registry's enforcement state reads as `managed` under
`rust/routing.rs:94-113`. *The caller's principal* is the principal `local_authority` derives from
the process's own effective UID (`rust/routing.rs:259-291`), or the broker's sealed context when
the broker operation protocol carries the call; never a request field. *A worker call* is any
invocation made with `KANBAN_WORKER_CREDENTIAL` set in its environment.

### Workers

**IDENT-01** — Worker identity exists only under managed enforcement.
Strength: `MUST` · Layer: `process` · Source: `a-75f7280c`.
`When the canonical registry is absent or its enforcement state is direct or prepared, every
worker verb, and every other command run with KANBAN_WORKER_CREDENTIAL set, is refused with
"worker identity needs managed enforcement; this installation is <state>", where <state> is
direct, prepared or "unregistered", and writes nothing.`

**IDENT-02** — A worker record carries a minted id and immutable labels.
Strength: `MUST` · Layer: `process` · Source: `t-2aafd55c` ("immutable worker/run/parent
identity"); `e-3a942ae9` ("Worker fields supplement rather than replace lane/actor").
`kanban worker register --run RUN --lane-actor ACTOR [--harness-agent ID] [--task TASK]
--grant GRANT... [--json] creates one worker with: workerId "w-" followed by a v4 UUID, minted by
Kanban; principalId, the caller's principal; parentWorkerId, the worker id of the credential the
caller presented, or null; runId RUN and harnessAgentId ID, each 1-128 ASCII characters, starting
with a letter or digit, then letters, digits, dot, underscore, colon or hyphen; laneActor ACTOR,
of the form @:<team>/<board>/<lane> with three non-empty segments and at most 128 characters;
taskRoot (IDENT-04); grants (IDENT-04); state "active"; registeredAt and registeredEpoch. No field
of a record ever changes except state and retiredAt. runId, harnessAgentId and laneActor are
accountability labels: no authorization decision reads them except the IDENT-08 lane actor
equality.`
`Failure behaviour: a malformed label is refused naming the flag and the rule; nothing is
written.`

**IDENT-03** — Registration and retirement are policy events.
Strength: `MUST` · Layer: `process` · Source: ADR-038 clause 4 (events are truth); ADR-029.
`worker register and worker retire each append exactly one chained policy event (kinds
"worker_registered" and "worker_retired") and advance the policy epoch by one, through the same
epoch check every policy mutation uses, so a context minted before the event is refused at commit.
The event names the worker id, the principal id, the parent worker id and, for registration, the
full grant set and task root; it never holds a credential or its digest. A refused registration
appends one denied-attempt audit row, as other denied policy mutations do, and does not advance
the epoch.`

**IDENT-04** — A worker's grant and task root only narrow its parent's.
Strength: `MUST` · Layer: `process` · Source: `a-ed2bbd7d`; `e-3a942ae9` ("Nested delegation
cannot widen ancestor scope").
`Each --grant is CAPABILITY=ATOM[,ATOM...], where CAPABILITY is read, write or admin
(rust/policy.rs:23) and the atoms form exactly one scope tuple under ScopeTuple::from_atoms
(rust/policy.rs:83-122); the registry tuple is refused for a worker. Registration is refused with
"denied or not found" unless every requested pair is satisfied (rust/policy.rs:558-574) by the
registrant's effective authority (IDENT-06; for a principal, its own authority). --task TASK names
one task on a board the grant covers; when the registrant is a worker with a task root, TASK must
be that root or one of its descendants by parent_id, else the registration is refused with "task
<TASK> is outside worker <parent>'s task root <root>". A worker without --task inherits its
parent's task root, or none.`

**IDENT-05** — A worker proves itself with a principal-bound credential.
Strength: `MUST` · Layer: `process` · Source: `t-2aafd55c` ("registration authority and spoofing
refusal"); `a-75f7280c` ("cannot impersonate managed delegation").
`Registration prints the credential exactly once, as "credential" in the JSON receipt: the string
"kwc_" followed by 64 lowercase hexadecimal characters from 32 random bytes. The registry stores
only its SHA-256 digest. A process presents a credential only through the environment variable
KANBAN_WORKER_CREDENTIAL; no flag, MCP argument or JSON field accepts one. A worker call is refused
with "denied or not found" when the credential is malformed, matches no worker, matches a worker
in any state other than active or with any retired ancestor, or matches a worker whose principalId
differs from the caller's principal. No command prints a credential or its digest after
registration.`

**IDENT-06** — Effective worker authority is recomputed on every call.
Strength: `MUST` · Layer: `process` · Source: `a-ed2bbd7d` ("intersect worker grant with parent and
task scopes").
`For each worker call, Kanban computes the worker's effective authority: for every tuple t in the
worker's own grant map G (rust/policy.rs:536-550), E(t) is the greatest capability c <= G(t) that
the parent's effective authority satisfies at t under rust/policy.rs:558-574, and t is absent when
no capability qualifies; the parent of a root worker is the principal's live authority
(rust/policy.rs:1602-1605). The worker's row checks are check_read and check_write
(rust/authz.rs:79-110) against E, with the task root as an extra requirement on every task the
call touches. Nothing computed in an earlier call is reused, so a principal grant revoked, or an
ancestor retired, takes effect on the next call. A check that E does not satisfy is refused with
"denied or not found" and writes nothing.`

**IDENT-07** — Retiring a worker retires its descendants' authority.
Strength: `MUST` · Layer: `process` · Source: `e-3a942ae9` ("expire/reassign fences the old worker
from writes and result acceptance").
`kanban worker retire WORKER is allowed to the worker's principal acting without a credential, to
any ancestor worker, and to the worker itself, and is refused with "denied or not found" to anyone
else. It sets the worker's state to retired. From the next call on, the retired worker and every
descendant are refused under IDENT-05. Leases a retired worker holds are not released or
transferred: they stay until they expire or the coordinator moves the task with --force, as any
abandoned lease does today (rust/store.rs:3451-3475). Retiring a retired worker is refused with
"worker <id> is already retired".`

### Leases

**IDENT-08** — A worker's claim or handoff accept is bound to the worker and stamped.
Strength: `MUST` · Layer: `process` · Source: `t-2aafd55c`; `e-3a942ae9` ("atomically claim exact
ready task with registered worker binding").
`A worker call to claim (by id or --next) requires --as to equal the worker's laneActor exactly,
else it is refused with "worker <id> acts as <laneActor>, not <as>". It considers only tasks for
which check_write against E (IDENT-06) and the task root pass, and refuses a named task outside
them with "denied or not found". On success the lease row records workerId and principalId besides
today's columns, and the claim receipt shows workerId and attempt (IDENT-09). Every checkpoint,
handoff and task event written under a worker-bound lease records the same workerId and
principalId. A claim made without a credential under managed enforcement records principalId and a
null workerId; under direct or prepared enforcement both are null.`
`A worker call to handoff accept applies the same --as rule and the same refusal first, then the
baseline's target rule unchanged (rust/store.rs:9658-9669, refusal "handoff <id> targets
<target>, not <as>"), then check_write against E and the task root on the handoff's task. The
lease it creates records the accepting worker's workerId and principalId, exactly as a worker's
claim does.`

**IDENT-09** — Each task counts its claim attempts.
Strength: `MUST` · Layer: `process` · Source: `t-2aafd55c` ("task attempt/generation").
`Every task has an attempt counter starting at 0. Each successful claim and each accepted handoff,
in every enforcement state, increments it by one inside the same transaction and stores the new
value on the lease row as attempt. The claim and handoff-accept receipts show it, task show shows
the current lease's attempt, and each checkpoint records the attempt of the lease it was written
under. A refused claim does not change the counter. The counter never decreases.`

**IDENT-10** — Lease-bound worker calls keep exact fencing and recheck authority.
Strength: `MUST` · Layer: `process` · Source: `t-2aafd55c` ("exact-token fencing"); `e-3a942ae9`
("expire/reassign fences the old worker").
`heartbeat, release, checkpoint, note and handoff create made as a worker call first apply
require_lease and its existing refusals unchanged (rust/store.rs:3266-3291), then require the
lease's workerId to equal the caller's worker id, else "lease belongs to worker <holder>, not
<caller>", then apply check_write against E (IDENT-06) over the task's current tags, together with
the task root. A lease taken without a credential cannot be used by a
worker call, and a worker-bound lease cannot be used by a call without a credential; each is
refused with that sentence, naming "no worker" for the side without one. No refusal names a lease
token or a credential.`

**IDENT-11** — Workers run a closed list of operations.
Strength: `MUST` · Layer: `process` · Source: `a-ed2bbd7d` ("explicit policy mapping");
`t-2aafd55c` ("arbitrary ledger mutations ... remain coordinator-only").
`A worker call may run only: a command whose generated MCP tool carries readOnlyHint true
(rust/mcp.rs:316), each row it reads checked against E; claim; heartbeat; release; checkpoint;
note, only on a task whose active lease the worker holds; handoff create, from a lease it holds;
handoff accept, under IDENT-08; worker register, worker show, worker list
and worker retire, for itself and its descendants. Every other command, including task add, task
move, task update, task remove, transact, sitrep, attention, subscription, deploy and every access
command, is refused with "worker <id> may not run <command>: it is coordinator-only" before any
other check, and writes nothing.`

**IDENT-12** — A worker without write holds no lease.
Strength: `MUST` · Layer: `process` · Source: `e-3a942ae9` ("Read-only workers get identity and
assignments but no unnecessary write lease").
`A worker whose effective authority grants write on no board scope may run the read commands of
IDENT-11, and may be the assignee of a task, but a claim or handoff accept by it is refused with
"denied or not found" and creates no lease.`

**IDENT-13** — Leases stay per task; nothing serializes a lane or a parent.
Strength: `MUST` · Layer: `process` · Source: `e-3a942ae9` ("not one lane-global busy flag"; "one
group coordinator must not serialize its sibling task workers").
`Two workers with the same laneActor, or with the same parent, each claiming a different ready task
at the same time, both succeed and hold distinct leases with distinct workerIds. Two workers
claiming the same task at the same time produce exactly one lease; the other is refused with the
existing already-claimed sentence, which begins "task <id> is already claimed by <holder> until
<expiry> (epoch ms)" (rust/store.rs:7516-7524). Holding a lease never blocks a claim on another
task.`

### Requests

**IDENT-14** — Claim-path requests are idempotent under a request id.
Strength: `MUST` · Layer: `process` · Source: `e-3a942ae9` ("Idempotent retries return the same
assignment and do not mint duplicate leases"); `t-2aafd55c` ("idempotent requests").
`claim, checkpoint and handoff create accept --request-id KEY, 16-128 ASCII characters of
letters, digits, dot, underscore or hyphen, in every enforcement state. Kanban stores, in the same
transaction as the write, a receipt keyed by (principalId or none, workerId or none, command, KEY)
holding the SHA-256 of the request's normalized arguments and the exact bytes of the response.
A later call with the same key and the same normalized arguments writes nothing and prints those
stored bytes, exit 0, even if the lease it named has since ended. The same key with different
arguments is refused with "request <KEY> was already used for a different <command> request" and
writes nothing. A call that was refused stores no receipt.`
`Data rules: the normalized arguments exclude --json and --request-id; a receipt is kept as long
as the board.`

### Surfaces

**IDENT-15** — Worker listings are bounded and never leak secrets.
Strength: `MUST` · Layer: `process` · Source: `t-2aafd55c` ("bounded output schemas").
`kanban worker list [--limit N] [--state active|retired|all] [--json] prints the caller's
principal's workers (as a worker call: itself and its descendants), newest first, at most N rows
(default 50, maximum 500; any other N is refused with "--limit must be between 1 and 500, got N"),
and when more exist ends with "truncated": true under --json. worker show WORKER prints one record
under the same visibility; a worker outside it is "denied or not found". Neither prints a
credential or a credential digest.`

**IDENT-16** — Worker verbs and request ids are in the one command table.
Strength: `MUST` · Layer: `process` · Source: ADR-010, ADR-011; `rust/mcp.rs:221-330`.
`The generated MCP manifest carries worker_register, worker_show, worker_list and worker_retire,
with readOnlyHint true only on worker_show and worker_list, and the request-id argument on claim,
checkpoint and handoff_create. An MCP tool call inherits the server process's environment, so a
server started with KANBAN_WORKER_CREDENTIAL acts as that worker for every call (rust/mcp.rs:
404-431). A refusal returns isError true with the CLI's sentence.`

### Compatibility

**IDENT-17** — The migration is versioned and keeps legacy rows' behaviour.
Strength: `MUST` · Layer: `process` · Source: `a-ed2bbd7d` ("migration for lease-bound scope");
`t-2aafd55c` ("versioned migration").
`Registry schema 15 adds the worker records. Board schema 38 adds the task attempt counter, the
attempt, workerId and principalId columns on leases, the workerId, principalId and attempt columns
on checkpoints and handoffs, and the request receipts. Migrating a board sets the counter to 1 and
the lease's attempt to 1 for each task with a lease row, and 0 otherwise, and leaves workerId and
principalId null on every existing row. A lease with a null workerId keeps the baseline's
principal-level behaviour until it ends. A binary at the baseline opening a migrated board or
registry is refused with its existing "database version N is newer than supported version M"
sentence (rust/db.rs:3383-3390).`

**IDENT-18** — Direct mode runs unchanged and claims no worker identity.
Strength: `MUST` · Layer: `process` · Source: `a-75f7280c` ("direct-mode legacy claims remain
compatible").
`Under direct or prepared enforcement, with KANBAN_WORKER_CREDENTIAL unset, every command behaves
as at the baseline apart from IDENT-09's attempt counter and IDENT-14's optional request id: the
same accepted inputs, the same refusals and the same written columns, with null workerId and
principalId. No output under direct or prepared enforcement shows a workerId, and an --as value
that looks like a worker id or a laneActor grants nothing.`

## 4. Acceptance examples

The managed examples run as a non-root UID with the principal `alice` holding `write` on
`board:B` (`B` the test board's id), as `tests/authz_bypass_matrix_e2e.rs` sets up; `C` names the
coordinator, a process with no credential.

### A1 (IDENT-01, IDENT-09, IDENT-18)

*Given* a fresh data root with no registry, and separately a registry in state `prepared`,
*when* the caller runs `worker list --json`, then `task list` with `KANBAN_WORKER_CREDENTIAL` set to
any value, then `claim T --as @:t/b/driver --json` with it unset,
*then* the first two are refused with `worker identity needs managed enforcement; this
installation is unregistered` (respectively `prepared`), and the claim succeeds with the
baseline's receipt plus `attempt` 1 and no `workerId`.

### A2 (IDENT-02, IDENT-03, IDENT-05)

*Given* managed enforcement,
*when* C runs `worker register --run run-1 --lane-actor @:t/b/driver --grant write=board:B --json`,
*then* the receipt holds a `w-` worker id, `parentWorkerId` null, `principalId` alice's id and a
`kwc_` credential; the policy epoch rose by one and the newest policy event is
`worker_registered` naming that worker and no credential; `worker show` on it prints no
credential; and a call with that credential run as a second non-root principal `bob` is refused
with `denied or not found`.

### A3 (IDENT-04, IDENT-06)

*Given* A2's worker `w1` (credential `k1`),
*when* a `k1` call registers a child with `--grant admin=board:B`, then with
`--grant write=board:B,*`, then with `--grant read=board:B --task T2` where T2 is a task,
and then the operator revokes alice's `write` on `board:B`,
*then* the first two are refused with `denied or not found` and append denied-attempt audit rows
without advancing the epoch, the third succeeds as `w2`, and after the revocation a `k1` claim of
a ready task is refused with `denied or not found`.

### A4 (IDENT-08, IDENT-09, IDENT-10)

*Given* `w1` and a ready task T,
*when* a `k1` call runs `claim T --as @:t/b/driver-2`, then `claim T --as @:t/b/driver --json`,
then `checkpoint T --lease L ...` without a credential, then the same with `k1`,
*then* the first is refused with `worker w1 acts as @:t/b/driver, not @:t/b/driver-2`, the second
returns a lease with `workerId` w1 and `attempt` 1, the third is refused with `lease belongs to
worker w1, not no worker`, and the fourth writes a checkpoint stamped with w1, alice and attempt 1.

### A5 (IDENT-05, IDENT-07, IDENT-11, IDENT-12)

*Given* `w1` holding T's lease, and `w2` from A3 (grant `read` only),
*when* a `k1` call runs `task move T done --as @:t/b/driver`, a `k2` call claims a ready task, C
runs `worker retire w1`, and then a `k1` and a `k2` call each run `task list`,
*then* the move is refused with `worker w1 may not run task move: it is coordinator-only`, the `k2`
claim with `denied or not found` and no lease row appears, retirement succeeds, both later calls
are refused with `denied or not found`, and T's lease is still held by w1 until it expires.

### A6 (IDENT-13)

*Given* two workers `w3` and `w4` registered by C with the same `--lane-actor` and ready tasks T3
and T4,
*when* both claim at once — w3 T3 and w4 T4 — and then both claim T5 at once,
*then* both first claims succeed with distinct workerIds and leases, and of the T5 claims exactly
one succeeds and the other is refused with the already-claimed sentence.

### A7 (IDENT-14)

*Given* a ready task T, in direct mode and again under `k1`,
*when* the caller runs `claim T --as @:t/b/driver --request-id req-000000000001 --json` twice,
then `claim T --as @:t/b/driver --lease-minutes 30 --request-id req-000000000001`,
*then* the second run prints byte-identical output to the first and the board has one lease and
attempt 1, and the third is refused with `request req-000000000001 was already used for a
different claim request`.

### A8 (IDENT-15, IDENT-16)

*Given* C has registered 501 workers,
*when* C runs `worker list --json`, then `worker list --limit 501`, and an MCP client lists tools
and calls `worker_list`,
*then* the first prints 50 rows and `"truncated": true` with no credential or digest, the second is
refused with `--limit must be between 1 and 500, got 501`, and the manifest lists the four worker
tools with `readOnlyHint` true only on `worker_show` and `worker_list`.

### A9 (IDENT-09, IDENT-17)

*Given* a board at schema 37 with one leased task and one unleased task, and a registry at 14,
*when* this slice's binary opens them, then a baseline binary opens them,
*then* the leased task shows attempt 1 and the unleased task's next claim gets attempt 1, the old
lease's holder can still heartbeat it without a credential, and the baseline binary is refused
with `database version 38 is newer than supported version 37`.

### A10 (IDENT-08, IDENT-11, IDENT-14)

*Given* `w1` holding T's lease `L` (attempt 1), and a worker `w5` registered by C with
`--lane-actor @:t/b/driver-2 --grant write=board:B` (credential `k5`),
*when* a `k1` call runs `handoff create T --lease L --as @:t/b/driver --to @:t/b/driver-2
--summary s --intent i --request-id hand-00000000001 --json` twice, then a `k5` call runs
`handoff accept H --as @:t/b/driver` and then `handoff accept H --as @:t/b/driver-2 --json`,
where `H` is the created handoff,
*then* the two creates print byte-identical output and the board holds one handoff, the first
accept is refused with `worker w5 acts as @:t/b/driver-2, not @:t/b/driver`, and the second
returns a lease on T with `workerId` w5 and `attempt` 2.

## 5. Contracts and data

- **Interface version or schema:** registry schema 15; board schema 38; CLI grammar
  `kanban worker register --run RUN --lane-actor ACTOR [--harness-agent ID] [--task TASK]
  --grant CAPABILITY=ATOM[,ATOM...]... [--json]`, `kanban worker show WORKER [--json]`,
  `kanban worker list [--limit N] [--state active|retired|all] [--json]`,
  `kanban worker retire WORKER [--json]`; `--request-id KEY` on `claim`, `checkpoint`,
  `handoff create`; environment variable `KANBAN_WORKER_CREDENTIAL`; MCP tools `worker_register`,
  `worker_show`, `worker_list`, `worker_retire` (IDENT-16).

  Example registration receipt (the only output that ever carries a credential):

  ```json
  {
    "workerId": "w-6f1c2d4e-2a1b-4c3d-9e8f-0a1b2c3d4e5f",
    "principalId": "p-...",
    "parentWorkerId": null,
    "runId": "run-1",
    "harnessAgentId": "IdentitySpecScout",
    "laneActor": "@:geoyws/kanban/driver",
    "taskRoot": null,
    "grants": [{"capability": "write", "scope": ["board:<id>"]}],
    "state": "active",
    "registeredAt": 1790858885480,
    "registeredEpoch": 42,
    "credential": "kwc_<64 lowercase hex>"
  }
  ```

- **Data invariants:** a worker's effective authority never exceeds its parent's, recursively to
  the principal's live authority; a worker record's identity fields never change; at most one
  lease per task; a task's attempt counter never decreases; a request receipt is written only in
  the transaction of the write it describes; no stored row holds a credential.
- **Migration:** IDENT-17. No data is converted into a worker; legacy leases end naturally.
- **Compatibility:** a baseline binary refuses the new schemas (IDENT-17). Direct and prepared
  installations see two additions only: the attempt counter and the optional request id
  (IDENT-18).
- **Ownership:** the operator owns principal grants; a coordinator owns the workers it registers
  and every coordinator-only operation; a worker owns only its leases and its descendants. The
  agent-policy exception that lets a subagent hold a credential belongs to dotfiles
  (ADR-059 Consequences).

## 6. Quality and security

- **Reliability:** registration, retirement, claims and receipts are single SQLite transactions;
  an idempotent replay is the retry story for a lost response (IDENT-14).
- **Accessibility:** N/A — no served markup.
- **Privacy:** credentials appear once, at registration, and never in listings, events, audit rows
  or refusals (IDENT-03, IDENT-05, IDENT-15). Labels are operator-chosen and hold no secret.
- **Security:** authority only narrows (IDENT-04, IDENT-06), is recomputed per call, and is bound
  to the kernel-derived principal (IDENT-05); unknown, foreign, retired and malformed credentials
  share one generic refusal, as policy denials do (`rust/authz.rs:34`); coordinator-only
  operations are refused by name before any row is read (IDENT-11). Residual risk, stated rather
  than hidden: any process of the principal's UID can read another process's environment or act
  as the principal without a credential; delegation limits what a credential can do, not what
  the principal can do (§2 non-goals).
- **Security — ASVS applicability:** OWASP ASVS 5.0.0 as verification guidance, not a
  certification claim. V8.2.1 and V8.3.1 apply to function- and data-level authorization checked
  server-side on every call (IDENT-06, IDENT-10, IDENT-11); V8.2.3 applies to the narrowing rule
  that prevents escalation through delegation (IDENT-04); V7.2.3 and V13.3 apply to the
  credential's entropy, one-time display and digest-only storage (IDENT-05); V16.2.1 applies to
  the chained policy events (IDENT-03).
- **Operability:** `worker list` and `worker show` are the coordinator's view of who it delegated
  to; every lease, checkpoint and handoff names its worker and attempt. Guidance, not a
  requirement: a lane that receives the IDENT-01 refusal should stop and report it rather than
  retry the call without its credential, which would run with the principal's full authority.
- **Performance:** no budget. Observation: each worker call reads the registry once more to walk
  the worker's ancestry, which is bounded by the delegation depth.

## 7. Open questions

None. The two material questions — where worker identity is enforced, and where a worker's grant
comes from — were taken by the owner on `a-75f7280c` and `a-ed2bbd7d` (2026-10-01). The remaining
shapes (credential format and channel, verb names, the operation list, the request-id scope, the
schema numbers) are wording choices under the decider, recorded with their rejected alternatives
in ADR-059.

## 8. Verification

Planned evidence for every mandatory requirement. The matrix section
(`## Requirements trace — docs/specs/identity.md IDENT-01..IDENT-18`) is the trace of record; this
table is its draft and the two land identical. No test exists yet: the implementing row
`t-2aafd55c` writes each named test in the change that implements it, so every row says
`no e2e coverage` plainly.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `IDENT-01` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A1). |
| `IDENT-02` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A2). |
| `IDENT-03` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A2, A3). |
| `IDENT-04` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A3). |
| `IDENT-05` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A2, A5). |
| `IDENT-06` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A3). |
| `IDENT-07` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A5). |
| `IDENT-08` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A4, A10). |
| `IDENT-09` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A1, A4, A9). |
| `IDENT-10` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A4). |
| `IDENT-11` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A5, A10). |
| `IDENT-12` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A5). |
| `IDENT-13` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A6). |
| `IDENT-14` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A7, A10). |
| `IDENT-15` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A8). |
| `IDENT-16` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A8). |
| `IDENT-17` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A9). |
| `IDENT-18` | MUST | process | `none` | no e2e coverage. Owed by `t-2aafd55c` (A1). |

## 9. Change log

- `2026-10-01` — slice created at `IDENT-01` .. `IDENT-18` under row `t-026ece7c`, after owner
  verdicts `a-eaa4d835` (slice admitted for `t-2aafd55c` only), `a-75f7280c` (managed-only
  delegation) and `a-ed2bbd7d` (broker registry grants). No supersessions yet.
- `2026-10-01` — independent `/quality spec` review (`ReviewIdentitySpec1`, commit `5d2f1a7`):
  SPEC-READY with six wording findings, applied without changing any requirement's meaning.
  IDENT-01 loses its unenforceable no-retry sentence, which moves to §6 operability as guidance.
  Acceptance headings A1, A5 and A9 name every requirement they prove. IDENT-08 states the
  handoff-accept rule (same `--as` rule, then the baseline target rule) and new example A10
  exercises it with a handoff-create replay. IDENT-10 names its check (`check_write` against E
  over the task's current tags). §1 lists ADR-029. IDENT-13 quotes and cites the already-claimed
  sentence.
