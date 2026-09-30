# Specification: two boards own one joint feature with bounded claims and delivery evidence (slice LINKED)

## 1. Identity and baseline

- **Slice ID:** `LINKED`. Requirement IDs are `LINKED-01` .. `LINKED-25`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Original baseline:** `2026-09-29` at commit `361d7e3` on branch
  `wt/t-fe137b57-land` (spec content merged at `3cb07ed`; code tree identical
  then). **Current citation baseline:** lane commit `521c19e` merged on
  2026-10-01; present-tense source and test line citations below were
  refreshed against that tree. Dated change-log entries retain their old lines.
- **Status:** `SPEC-READY` on 2026-09-26 (independent gate by a reviewer applying the SDD §1 exit criteria over four rounds; eight findings closed — LINKED-09 merged to one trace row, resumption bound to the existing `claim --session`/`handoff accept --session` entry points, revocation stated as LINKED-14's authorized exit with audited UNFREEZE and release-only revoked leases, the ordered pair's order and compensation defined in LINKED-25 with no dependency on `e-df626704`, §7 heading restored, LINKED-02 repaired, `none`-row owners grounded in the parent's work packages with `--list` enumeration recorded, and the release citation corrected to `rust/lib.rs:169`). It authorises neither implementation nor release.
- **Revision status (2026-09-30 owner decision `a-53b18f9a`):** the earlier
  `SPEC-READY` gate predates the LINKED-23 withdrawal. This revision is pending
  a fresh independent `/quality spec` review; the withdrawn ID remains reserved.
  <!-- SPEC-READY is stamped here by an independent reviewer against the SDD reference's §1 exit
  criteria, not by the writer of this document. It authorises neither implementation nor rollout
  nor release. -->
- **Owner (product scope):** George. He alone resolves scope, whether a Non-goal is reinstated,
  and the open questions in §7.
- **Decider (wording of this document):** the `t-fe137b57` writer.
- **Sources:**
  - Epic `e-73bf760f` (status `todo`) — the accepted scope this specification states as
    requirements: each board owns its epics/tasks for joint Unum/Acies features; both boards
    expose the same relationship, who worked each task, and which repo commits delivered it; a
    worker bound to the joint feature may claim ONLY explicitly selected tasks. Sibling tasks stay
    `draft`.
  - George, 2026-09-25: the bounded scope above was approved in resolved attention `a-93efef34`
    (`e-73bf760f` + spec task `t-fe137b57` to `todo`, siblings stay `draft`).
  - `docs/adr/ADR-051-joint-work-is-a-registry-relation-with-a-bounded-selected-set.md` — the
    decision this specification implements. §1 (one stored relation, exposed from either side),
    §2 (the selected set is its own registry record, not an epic and not a sprint), §3 (the
    ordered cross-registry pair, never an atomic span) and §4 (the four evidence roles plus the
    non-code disposition) are binding on every requirement below.
  - `e-df626704` (`todo`, cross-board readiness gates) — the COMPLEMENTED sibling: it owns the
    `depends-on` resolver, its authorization, and its locks for its own slice. This
    specification shares only the commissioned `(boardID, id)` spelling and defines its own
    per-half authorization and write-time serialization (LINKED-11, LINKED-25); it takes no
    implementation dependency on the sibling.
  - `docs/adr/ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md` — plans stay epics; the
    selected work set is therefore not an epic and joint planning stays where it is.
  - `docs/adr/ADR-041-transact-is-one-atomic-ordered-write-batch.md` — `transact` is one atomic
    ordered batch on ONE board; it is not a cross-registry atomic primitive (LINKED-25).
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — why every refusal
    below names its fix, and why an ambiguous cross-board reference is refused rather than ranked.
  - `docs/adr/ADR-029-audit-journals-are-hash-chained-and-externally-anchored.md` — the chain the
    membership revisions and contribution receipts join.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §9 is why §6 carries no invented budget.
- `docs/PRD.md:9`-`docs/PRD.md:12` (one durable place to coordinate atomic ownership and
  preserve evidence) and `docs/PRD.md:32` (prevent concurrent ownership) — the product-level
  statements these requirements refine. Neither document changes for this slice (see §5).
- Shipped surface at the current citation baseline: `rust/lib.rs:158` (`claim [ID | --next] --as AGENT`),
  `rust/lib.rs:167` (`claim --candidates`, read-only), `rust/lib.rs:186` (`handoff accept`),
  `rust/lib.rs:125`-`rust/lib.rs:127` (`task add --depends-on`, local scalars),
  `rust/lib.rs:332` (`claim` has no `--force`), `rust/model.rs:380`
  (`RELATION_KINDS`, exactly `parent`, `ancestor`, `depends-on` — companion is none of them).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section is the trace of record and the two are kept identical by the same
  change.

## 2. Purpose and scope

**Intended outcome.** Two boards — one Unum, one Acies — describe the same joint feature without
merging: each board keeps owning its own epics and tasks, both boards expose the identical
relationship, the identical attribution of who worked each task, and the identical delivery
evidence of which repo commits delivered it, and a worker bound to the joint feature can claim
only the tasks explicitly selected for it.

**Users / actors.**

- George, the operator, who pairs two boards for a joint feature, selects its work set, and
  closes it on exact evidence.
- Agent lanes bound to the joint feature (`--as AGENT`, with lane and session), which claim,
  hand off, resume through the existing `--session` entry points, and record contributions only
  inside the selected set, through the CLI and the in-binary MCP server. (The served UI this
  line named on 2026-09-26 is retired — ADR-053 — so LINKED-23 is withdrawn, not re-homed.)
- The authoritative registry (on `hax`), which owns the shared records; the two boards, which
  keep owning their tasks, states, and leases.

- **In scope.** The one stored companion relation and its two-sided exposure; the selected work
  set with audited membership revisions; the single claim gate across candidates, next/named
  claims, lease-taking handoffs, and session resumption through the existing `--session` entry
  points (`rust/lib.rs:158`, `rust/lib.rs:186`); the append-only contributions and integration
  receipts with their evidence roles; the CLI/MCP agreement over that surface.

**Boundaries.**

- The `depends-on` resolver, its authorization, and its locks are owned by `e-df626704`
  (`todo`), not by this slice. This slice shares only the commissioned `(boardID, id)` spelling
  with it and defines its own per-half authorization and write-time serialization (LINKED-11,
  LINKED-25); it never restates the sibling and never waits on it.
- Ownership, states, and leases stay on the existing boards. The registry owns shared records
  only.
- The release receipt's schema is ADR-039's and ADR-044's; this slice states what an integration
  receipt must prove, not where the release manifest keeps it.
- Display names, board paths, and `@#` tokens are conveniences at the edge. They are NEVER
  persisted identity (LINKED-02).

**Non-goals.**

- **No merged board and no copied tasks.** The two boards are not fused and no row is duplicated
  across them; there is exactly one stored relation read from either side (LINKED-01).
- **No readiness recomputation and no status copying.** Whether a readiness prerequisite is
  satisfied is the sibling slice's rule to apply, not this one's; this slice never copies a
  status from one board to the other.
- **No cross-registry atomic write.** `transact` stays single-board (ADR-041); the ordered pair
  in LINKED-25 is durable and fail-closed, not atomic.
- **No second read path past `Store`.** Every new projection reads the same `Store` methods the
  CLI calls.
- **No invented performance or availability commitment.** §6 records no budget. Only George may
  turn a measurement into one (ADR-047 §9).

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange against `kanban serve`, no browser. Retired with
  the serve surface (ADR-053); it names no active LINKED requirement and is kept here only so
  the withdrawn rows stay readable.
- `chrome` — a compiled-binary end-to-end test driving real Chrome. It named only the
  withdrawn LINKED-23; it names no active LINKED requirement.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

IDs are assigned in creation order and never reused; the groups below are topical.

### Shared records

**LINKED-01** — Store one companion relation, expose it from either side.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope; ADR-051 §1.
`A companion pairing between one Unum item and one Acies item is stored exactly once in the
authoritative registry and is readable from either board's side with identical content: the same
pair, the same attribution, the same evidence.`
`Permissions: the operator (or a principal the registry authorizes for pairing) may create and
retire pairings; a bound worker may read but never write them.`
`Failure behaviour: a pairing write the actor may not make is refused before any row lands, with
the refusal naming the missing authority; nothing is half-written.`
`Data rules: one stored row per pairing; no mirrored copy on either board; the row survives a
restart byte-identical.`

**LINKED-02** — Identify shared records by board UUID plus exact item ID only.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope; approved scope (George,
2026-09-25, `a-93efef34`) for the UUIDs and the `(boardID, id)` shape; ADR-051 §1.
`Every shared record names its endpoints as `(boardID, id)` where `boardID` is the stable board
UUID (Acies `3818ff5a-579e-4415-a5b5-0187252bc81d`, Unum `85a751dd-ee19-4d34-8b6d-62c9a9f46ba6`)
and `id` is the exact item ID; display names, board paths, and `@#` tokens are conveniences,
never persisted as identity and NEVER resolved as identity at write time.`
`Permissions: any reader may use the conveniences for display; no writer may address a shared
record by them.`
`Failure behaviour: a write naming an endpoint by display name, path, or token is refused with a
sentence naming the required `(boardID, id)` shape; nothing is written.`
`Data rules: stored endpoints carry UUIDs and exact IDs only; no name/path/token column exists
on a shared table.`

**LINKED-03** — Keep the two companion edges symmetric.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope; ADR-051 §1.
`Creating the pairing from the Unum side exposes the identical pairing from the Acies side, and
retiring it from either side retires both exposures in the same change: at no observable moment
does one side show a live companion the other side denies.`
`Failure behaviour: a one-sided exposure observed from either side is a defect in the write, not
a state to reconcile later; the retiring write that leaves one edge live is refused whole.`
`Data rules: the symmetry is a property of the one stored row, not of two rows kept in step.`

**LINKED-04** — Pin endpoints to incarnations so rename, retirement, and recreation cannot retarget.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope; ADR-051 §1.
`Each endpoint pins the incarnation it was paired with. Renaming a board, retiring a board or
item, or recreating an item under a reused ID never moves an existing pairing onto the new
meaning of the name: the pairing keeps pointing at the pinned incarnation and reads as
dangling-or-retired until explicitly re-paired.`
`Failure behaviour: resolving a pairing against a retired or recreated endpoint answers the
endpoint's retired/recreated state, never the new row's content; no silent retarget.`
`Data rules: the incarnation pin is stored with the pairing and is never updated by a rename,
retire, or create.`

**LINKED-05** — Survive a restart with both directions identical.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance A).
`After the registry and both boards close and reopen — including a process restart between the
pairing write and the read — the pairing, its attribution, and its evidence read identically
from the Unum side and the Acies side.`
`Data rules: everything §3's shared-record group asserts is durable state, never session state.`

**LINKED-06** — Refuse unknown or denied endpoints without partial writes or leakage.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance A); ADR-008.
`Pairing, reading, or attaching evidence to an endpoint the caller may not resolve — unknown
board UUID, unknown item ID, retired board, or an item on a board the caller may not read — is
refused before any row lands, with one sentence that names neither the absent row's content nor
which of unknown/denied/retired was the cause where telling them apart would leak existence.`
`Permissions: a caller lacking read on either endpoint may pair, read, or evidence neither side.`
`Failure behaviour: the exact refusal sentence; zero rows written; zero bytes of the hidden
endpoint's content in the receipt.`
`Data rules: a refused write leaves the registry and both boards byte-identical to before it.`

### Bounded claims

**LINKED-07** — Bind the worker to the selected set; outside it, no claim.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope; ADR-051 §2.
`A worker bound to the joint feature carries a durable binding to its selected work set, and may
claim ONLY tasks explicitly in that set. The binding survives process restarts and lane
reattachment; it ends only by expiry, release, or an authorized exit/rebind (LINKED-14).`
`Permissions: the bound worker may claim selected tasks; any claim outside the set is refused
whoever the worker is, including the operator's own lanes.`
`Failure behaviour: an out-of-set claim is refused with a sentence naming the selected set and
the refused task; no lease is granted and no event is appended.`
`Data rules: the binding and the set revision it was checked against are recorded on every
granted claim's event.`

**LINKED-08** — Hold one claim gate across every claim path.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance B); ADR-051 §2.
`Candidates (`rust/lib.rs:167`), `--next` and named claims (`rust/lib.rs:158`), lease-taking
handoff acceptance (`rust/lib.rs:186`), and resumption share one selected-scope gate. Resumption
is exactly the two existing entry points presenting `--session`: a `claim` with `--session ID`
(`rust/lib.rs:158`) and a `handoff accept` with `--session ID` (`rust/lib.rs:186`) — no new verb
exists for resuming, and the field shape is unchanged. What the gate refuses by one path it
refuses by all of them, and the `claim --candidates` read never offers a row the atomic claim
path would refuse.`
`Failure behaviour: the same refusal sentence on every path, naming the selected set; a handoff
accepted outside the set stays `pending` and grants no lease.`
`Data rules: the gate reads the same binding revision on every path; no path caches a stale
membership.`

**LINKED-09** — Keep every existing gate holding.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance B); ADR-051 §2.
`The selected-scope gate is added beside the existing gates, never instead of them: atomic
single-claimer ownership, the read-only scheduler view, model allow-lists, sprint boundaries,
driver scope, assignee gates, dependency readiness, and cycle rejection all still refuse exactly
as today, and where two gates refuse, the refusal names both.`
`Failure behaviour: any existing gate's refusal is unchanged in sentence and effect; the scope
refusal never masks which existing gate also refused.`

**LINKED-10** — Never let descendants silently join the scope.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance B); ADR-051 §2.
`Selecting a task does not select its children, its parent, its epic-mates, or its `depends-on`
neighbours: only explicitly selected item IDs are claimable, and adding a child requires its own
audited membership revision (LINKED-12).`
`Failure behaviour: a claim on an unselected descendant is refused as out-of-set even while its
selected ancestor is held; the refusal names the missing explicit selection.`

**LINKED-11** — Serialize revocation against claims and race membership writes.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance B); ADR-051 §3.
`Revocation of a worker's binding and that worker's in-flight claim race serialize: exactly one
wins, the loser observes the winner's state, and a claim granted before the revocation landed
keeps its lease while granting no further claims. Concurrent membership revisions to the same
selected set serialize on a write-time revision check — a revision written against a stale
revision number is refused whole and must be re-read and re-applied. Serialization uses the
store's single-writer transaction plus the write-time revision check defined here; no
cross-board lock primitive from any other slice is used or needed.`
`Failure behaviour: the stale-revision refusal names the current revision; the revoked-worker's
new claim is refused naming the revocation; neither path leaves a half-written lease or a
half-written set.`

**LINKED-12** — Audit every membership revision.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope; ADR-029.
`Every change to a selected set — add, remove, freeze, unfreeze, authorized exit (revocation),
rebind — appends one audited revision carrying its revision number, its author, its reason, and
the exact resulting member list; the chain is hash-chained with the registry's journals and
`audit verify` stays healthy across every revision.`
`Permissions: only the operator or a principal the registry authorizes for membership may write
revisions; the bound worker may read but never write them.`
`Data rules: revisions are append-only; no revision is edited or deleted; the current set is
always the latest revision.`

**LINKED-13** — Settle frozen versus revoked membership.
Strength: `MUST` · Layer: `process` · Source: deferred decision settled here; ADR-051 §2.
`A FROZEN set takes no new members and no new claims, while leases already granted keep running
to expiry, release, or handoff: freezing parks taking without stranding holding. Freezing is
reversible only by an audited UNFREEZE revision (LINKED-12); there is no silent thaw. A REVOKED
binding is the authorized exit in LINKED-14's third ending, recorded as an audited revision
stating the reason; it ends taking immediately, while leases already granted keep their
heartbeat until expiry or release — but those leases cannot take new work and cannot be handed
to another lane. The only permitted ending for such a lease is `release ID --lease TOKEN`
(`rust/lib.rs:171`), returning the task to its board's claimable set.`
`Failure behaviour: a claim against a frozen set is refused naming the freeze; a new claim or a
forward handoff on a revoked binding is refused naming the revocation; in both cases live leases
are untouched.`

**LINKED-14** — Bind actor, lane, and session; exit and rebind only by authority.
Strength: `MUST` · Layer: `process` · Source: deferred decision settled here; ADR-051 §2.
`A selected-scope binding names exactly one `(actor, lane, session)` triple, recorded at bind
time and checked on every claim path. Only three endings exist: the bound worker releases its
own binding; the binding expires; or the operator (or a principal the registry authorizes for
membership) exits or rebinds it with an audited revision stating the reason — this operator
exit is what LINKED-11, LINKED-12, and LINKED-13 call revocation. No lane inherits another
lane's binding by naming its agent, and no session resumes another session's binding without an
explicit rebind: after an exit, the old triple resumes only through a new audited revision, and
resuming the old session without one is refused.`
`Failure behaviour: a claim presenting a different triple than the binding holds is refused
naming the mismatch; a rebind without authority is refused whole with no binding changed.`
`Data rules: the triple and its ending, when ended, are durable on the binding record.`

### Delivery evidence

**LINKED-15** — Record structured append-only contributions with full identity.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance C); ADR-051 §4.
`Every contribution to a joint task is one append-only record carrying the repo identity, the
full object IDs (never short hashes), the actor, lane, and session, the task it is for, and the
evidence itself. Records are never edited and never deleted; a correction is a new record that
names the record it corrects.`
`Permissions: the lane that did the work records its own contributions; no lane records for
another lane's triple.`
`Failure behaviour: a contribution missing any identity field, or carrying a shortened object
ID, is refused naming the missing field; nothing is appended.`
`Data rules: contributions live in the registry beside the pairing they evidence and survive
restarts; short hashes are refused at write time, not expanded.`

**LINKED-16** — Distinguish the four evidence roles.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance C); ADR-051 §4.
`Each contribution declares exactly one role: the starting HEAD the work began from,
implementation commits the work produced, the accepted integration commit, or the tested
consumer candidate. A record carrying two roles, or none, is refused; a query for one role never
returns another.`
`Failure behaviour: the role refusal names the four roles and quotes the record's declared one;
nothing is appended.`

**LINKED-17** — Retain the merge/squash mapping.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance C); ADR-051 §4.
`Where the integration commit is a merge or a squash of implementation commits, the receipt
retains the explicit mapping from each implementation commit to the integration commit that
absorbed it. A squash whose mapping is absent reads as unevidenced, not as self-evident.`
`Failure behaviour: an integration commit that absorbs unmapped implementation commits satisfies
no deliverable until the mapping is recorded.`

**LINKED-18** — Prove the exact consumer commit over the exact dependency path.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance C); ADR-051 §4.
`A consumer-side proof names the exact consumer commit tested and the exact dependency path that
reached it, including every nested hop. A proof that names the commit but not the path, or a
path with a hop missing, satisfies nothing.`
`Failure behaviour: the refusal names the missing commit or the missing hop; the deliverable
stays open.`

**LINKED-19** — Never accept a moving pointer or a near-miss as proof.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance C); ADR-051 §4.
`A moving `latest` checkpoint is not proof of any deliverable. A wrong hash, a stale pin, a
wrong repo path, or a changed candidate cannot satisfy a deliverable, even when everything else
about the record matches.`
`Failure behaviour: each near-miss is refused with a sentence naming the exact mismatch —
expected hash versus presented hash, pinned versus presented candidate, expected versus
presented path — and the deliverable stays open.`

**LINKED-20** — Mark joint work only on exact evidence, and close only on all of it.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance C); ADR-051 §4.
`No task reads as jointly delivered until its deliverables each carry exact evidence under
LINKED-15..LINKED-19, and a joint feature closes only when every deliverable on both boards is
so evidenced. Partial evidence reads as partial, by deliverable, never as joint, and never by
extrapolation from one side's state.`
`Failure behaviour: a close attempted with any deliverable unevidenced is refused naming each
open deliverable; nothing closes and no evidence is rewritten.`

**LINKED-21** — Record non-code work with the same discipline minus the hash.
Strength: `MUST` · Layer: `process` · Source: deferred decision settled here; ADR-051 §4.
`Work whose deliverable is not code — a decision, a review, a document, an approval — is
recorded as a contribution with kind `non-code`, carrying the same actor/lane/session, task,
and evidence fields as LINKED-15, but exempt from the object-ID and role rules of
LINKED-15..LINKED-19: there is no commit to hash and no role to declare. A `non-code` record
never satisfies a code deliverable, and a code record never satisfies a `non-code` one.`
`Failure behaviour: a `non-code` record offered for a code deliverable is refused naming the
mismatch, and vice versa.`

### Surfaces and the sibling slice

**LINKED-22** — Agree across CLI and real stdio MCP.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance D).
`Every LINKED behaviour reachable through the CLI is reachable through the real stdio MCP server
with the same observable result — the same pairings, the same scope refusals in the same words,
the same attribution, the same evidence — because both adapters run the same `Store` entry
points (ADR-010's generated surface: one operation, one tool).`
`Failure behaviour: a behaviour the CLI refuses and MCP accepts, or sentences that differ
between the two, is a defect in the adapter, and the MCP side is fixed to match the CLI.`

**LINKED-23** — Show the same joint state in the served web UI. WITHDRAWN
2026-09-30 by George (`a-53b18f9a`, choice `withdraw`); ID reserved.
Original strength: `MUST` · Original layer: `chrome` · Original source:
`e-73bf760f` scope (acceptance D).
`The served surface this requirement named — kanban serve, the web SPA, and
every HTTP route — was retired with no successor (ADR-053, 2026-09-28). George
withdrew this obligation instead of re-homing it. The ID remains reserved and
must not be reused; the original 2026-09-26 wording below remains historical
trace, not a current requirement. A future exposure requires a new decision.`
`Original (no force): the served UI shows the same pairing, attribution, and evidence the CLI
reads: the same related item on both boards' pages, the same worker on each task, the same
commits behind each deliverable. Where the UI cannot render a state, it says so rather than
rendering a different one.`
`Original failure behaviour (no force): a page that shows a companion the CLI denies, or hides
evidence the CLI shows, is a defect in the projection, fixed to match the CLI.`

**LINKED-24** — Fail, recover, save, and reopen on the real compiled binary.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance D).
`Every LINKED write interrupted mid-flight — killed process, lost session, dropped connection —
leaves the registry and both boards in the last fully written state, and a saved session
reopened on the real compiled binary resumes exactly the bindings, leases, and evidence its
triple held: no replayed pairing, no duplicated contribution, no resurrected binding.`
`Failure behaviour: recovery replays nothing already written; a write whose receipt the caller
never saw is safe to retry and answers the stored result rather than writing twice.`

**LINKED-25** — Complement `depends-on`; never replace it, never span `transact`.
Strength: `MUST` · Layer: `process` · Source: `e-73bf760f` scope (acceptance B); ADR-041; ADR-051 §3.
`Companion is not `depends-on`: a companion edge never satisfies a readiness gate and never
appears in a `blockingGates` answer; scope membership is neither. A joint change is an ordered
pair of single-board writes: first, the write on the board that owns the acting lane's task,
read back with its revision; second, the write on the other board carrying the first write's
`(boardID, revision)` reference — attempted only if the first landed. Each half is authorized
exactly as if it arrived alone under that store's existing authorization; serialization is the
store's single-writer transaction plus the write-time revision check (LINKED-11), and no
cross-board lock primitive is used or needed. A `transact` batch never spans two boards, or a
board plus the registry: it is refused whole before anything runs, naming the single-board rule
(ADR-041). A refused or crashed second half leaves the first half intact and reported, and the
compensation is explicit and operator-run: an audited retire/revision of the first half citing
the failed second half's receipt — never a silent rollback and never a half-hidden write.`
`Failure behaviour: a `transact` naming two boards or the registry plus a board is refused whole
before anything runs, naming the single-board rule; a failed second half reports both halves'
states and the exact compensation revision the operator must run.`

## 4. Acceptance examples

### A1 — Pair two disposable boards; both directions read identical after a restart (LINKED-01, LINKED-02, LINKED-03, LINKED-05)

*Given* two disposable boards, one Unum and one Acies, each holding one `todo` task, and no
pairing between them,
*when* the operator pairs them by `(boardID, id)` on both ends, kills every process, reopens
both boards, and reads the pairing from the Unum side and from the Acies side,
*then* both reads show the same pair, and retrying the identical pairing write answers the
stored record rather than writing a second row.

### A2 — Unknown, denied, renamed, and recreated endpoints cannot move a pairing (LINKED-02, LINKED-04, LINKED-06)

*Given* a live pairing between a Unum task and an Acies task,
*when* a caller pairs by display name instead of `(boardID, id)`; reads through an unknown board
UUID; reads an item on a board it may not read; renames a board; retires the Acies task; and
recreates an item under the retired ID,
*then* the display-name write is refused naming the `(boardID, id)` shape, the unknown and
denied reads are refused without leaking which cause applied or any content bytes, and after the
rename, retirement, and recreation the pairing still points at the pinned incarnation — it never
reads the new row's content, and nothing half-written exists anywhere.

### A3 — A bound worker takes its selected task and is refused everywhere else (LINKED-07, LINKED-08, LINKED-10)

*Given* a worker bound to a joint feature whose selected set holds exactly one task `t-in`, with
a sibling task `t-out`, an unselected child of `t-in`, and a `depends-on` neighbour of `t-in`
beside it,
*when* the worker lists candidates, claims `--next`, claims `t-in` by name, claims `t-out` by
name, claims the child, claims the neighbour, takes a lease-taking handoff for `t-out`, and
resumes onto `t-out` twice — once through a `claim --session` and once through a `handoff
accept --session` (the two resumption entry points LINKED-08 names),
*then* candidates offer `t-in` and none of the others, the `t-in` claims succeed, and every
`t-out`, child, neighbour, handoff, and both resumption attempts are refused with the same
sentence naming the selected set — the child and the neighbour even while `t-in` is held.

### A4 — Revocation, races, freezes, and rebinds serialize under authority (LINKED-11, LINKED-12, LINKED-13, LINKED-14)

*Given* a worker bound as `(actor, lane, session)` holding a live lease, with the selected set
at revision 7,
*when* the operator revokes the binding mid-claim while the worker attempts a second claim; two
operators write membership revisions against revision 7 at once; the operator freezes the set
and the worker claims; and a different lane presents the same agent name to claim,
*then* exactly one of revocation and second-claim wins and the loser observes it, the live lease
keeps its heartbeat but takes nothing further, exactly one membership revision becomes revision
8 while the other is refused naming revision 8, post-freeze claims are refused naming the freeze
while the live lease runs on, and the lookalike lane is refused naming the triple mismatch —
and every change sits in the hash-chained revision log with author, reason, and full member
list, verifiable by `audit verify`.

### A5 — Evidence records the four roles, the squash mapping, and the nested path (LINKED-15, LINKED-16, LINKED-17, LINKED-18)

*Given* a joint task with one code deliverable, a repo at starting HEAD `H`, implementation
commits `C1` and `C2` squashed into integration commit `I`, and a consumer commit `K` reached
through dependency hops `D1` then `D2`,
*when* the lane records the starting HEAD, `C1` and `C2` as implementation commits, `I` with the
explicit `C1→I, C2→I` mapping, and `K` with the path `D1→D2`,
*then* each record carries repo identity, full object IDs, actor/lane/session, task, and its one
role; a record with no role or two roles is refused; and a correction to any record lands as a
new record naming the corrected one, with the original bytes unchanged.

### A6 — Near-misses prove nothing, partial evidence closes nothing (LINKED-19, LINKED-20, LINKED-21)

*Given* a joint feature with two deliverables, one code and one `non-code` review,
*when* the lane offers a moving `latest` pointer for the code deliverable; a right-length wrong
hash; a stale pin; the right hash at the wrong repo path; a changed candidate; a `non-code`
record for the code deliverable and a code record for the review; and then the operator closes
the feature with one deliverable still open,
*then* every near-miss is refused naming its exact mismatch, each cross-kind record is refused
naming the kind mismatch, the close is refused naming each open deliverable, and neither board
shows the feature — or any task of it — as jointly delivered.

### A7 — CLI and MCP agree, and recovery replays nothing (LINKED-22, LINKED-24)

*Given* a live pairing with a bound worker, one recorded contribution, and one scope refusal on
record,
*when* the same pairing read, attribution read, evidence read, and out-of-set claim are issued
through the CLI and through the real stdio MCP server; and the registry process is killed
mid-write and the lane's session is saved and reopened on the real compiled binary,
*then* CLI and MCP answer byte-identical results in identical words, and after recovery the
bindings, leases, and evidence are exactly what the triple held — the interrupted write
retried once answers the stored result instead of doubling it. (The two-boards'-pages Chrome
steps this example carried on 2026-09-26 left with LINKED-23: the served surface is retired,
ADR-053.)

### A8 — Companion never gates, and no batch ever spans boards (LINKED-09, LINKED-25)

*Given* a paired Unum/Acies task pair with a `depends-on` edge outstanding on the Unum side and
all of today's gates armed (lease held elsewhere, model allow-list, sprint boundary, assignee),
*when* a scheduler asks readiness, a `blockingGates` answer is read, a `transact` names both
boards, and the ordered pair's second half is killed after the first landed,
*then* the companion moves no gate and appears in no `blockingGates` answer, every existing
gate refuses exactly as today with both refusals named where two apply, the two-board batch is
refused whole before anything runs naming the single-board rule, and the killed second half
reports both halves' states with the compensation the operator must run — the first half intact
and reported, never silently rolled back.

## 5. Contracts and data

- **Interface version or schema:** the CLI grammar gains the pairing, membership, binding, and
  contribution verbs; every cross-board reference in flags, JSON, and events names `(boardID,
  id)` with `boardID` a UUID string and `id` the exact item ID — the spelling commissioned for
  both slices in the approved scope (George, 2026-09-25, `a-93efef34`); this slice defines that
  spelling from that source and takes no implementation dependency on `e-df626704` (`todo`).
  Local `--depends-on ID ...` scalars are unchanged (`rust/lib.rs:127`). Event payloads carry
  the binding triple, the set revision, the evidence role, and full object IDs. This settles
  the deferred protocol/CLI field-name decision: field shape only, no new verb is named here.
- **Data invariants:** one stored pairing row per companion (LINKED-01); endpoints are UUIDs plus
  exact IDs with an incarnation pin and no name/path/token column (LINKED-02, LINKED-04);
  symmetry is a property of that row (LINKED-03); the selected set is always its latest audited
  revision (LINKED-12); contributions are append-only with full object IDs and one role each
  (LINKED-15, LINKED-16); no `transact` spans boards (LINKED-25).
- **Migration:** the registry gains the pairing, selected-set, binding, and contribution tables
  with their indexes, following the established table-rebuild pattern; existing boards open
  unchanged with no joint state until first paired. `BOARD_SCHEMA_VERSION` moves only if a board
  table must change; otherwise the migration is registry-side alone.
- **Compatibility:** unpaired boards behave exactly as today; an older client that knows no
  pairing verb sees no pairing rows and is refused, not misled, when it addresses one; retired
  and recreated endpoints read as retired/recreated, never as their replacements (LINKED-04).
- **Ownership:** the registry owns pairings, selected sets, bindings, and contributions; the
  boards own tasks, states, and leases. `docs/BRD.md` and `docs/PRD.md` are left untouched by
  this slice: the BRD's business purpose (one durable ledger owning work state across both
  estates) and the PRD's product statements (atomic ownership, handoffs, evidence) already cover
  joint work, and this specification refines their behaviour without moving any business or
  product truth — the singleton rule (ADR-047 §1) asks for a citation, which §1 gives, not an
  edit.

## 6. Quality and security

- **Reliability:** every LINKED write is fail-closed with no partial state (LINKED-01, LINKED-06,
  LINKED-11, LINKED-24, LINKED-25); recovery replays nothing already written (LINKED-24).
- **Accessibility:** N/A — this slice adds no operator-facing markup of its own. There is no
  served page left for it to touch (ADR-053); LINKED-23, which required the projection to match
  the CLI rather than invent a rendering, is withdrawn with that surface.
- **Privacy:** contributions and receipts carry actor, lane, and session identifiers by design
  (LINKED-15); like every board read they are served only to callers authorized to read the
  board, and LINKED-06's refusal leaks neither content nor cause.
- **Security:** bindings check the full `(actor, lane, session)` triple on every claim path
  (LINKED-14); membership writes need membership authority and pairing writes need pairing
  authority (LINKED-01, LINKED-12); no confused-deputy addressing exists because display names,
  paths, and tokens are never identity (LINKED-02); served projections no longer exist
  (ADR-053), so there is no projection for a lease token to appear in, and CLI/MCP output
  rules on tokens are unchanged.
- **Operability:** membership revisions are auditable and `audit verify`-healthy (LINKED-12);
  failed ordered pairs report both halves and their compensation (LINKED-25); frozen sets park
  taking without stranding holding (LINKED-13).
- **Performance:** N/A — no observation is recorded here and no budget is set. Timing is proven
  the way the product always proves it: the same-change real E2E plus the release gate in Linux
  Docker (acceptance E), measured at release, never budgeted in this document (ADR-047 §9).

## 7. Open questions

None. The six deferred decisions are settled in this specification and ADR-051: the selected set
is its own registry record, not an epic and not a sprint (§5, LINKED-07); the binding triple and
its three endings, with revocation stated as the authorized exit and the old triple rebindable
only by a new audited revision (§3 LINKED-11..LINKED-14); frozen versus revoked, with freezing
reversible only by audited UNFREEZE and revoked leases ending only via `release` (§3 LINKED-12,
LINKED-13); the concurrency strategy — the store's single-writer transaction plus write-time
revision checks defined here, with `transact` staying single-board and the ordered pair's order
and compensation defined in LINKED-25, taking no implementation dependency on `e-df626704`
(§3 LINKED-11, LINKED-25); the evidence roles plus the `non-code` disposition (§3 LINKED-16,
LINKED-21); and the `(boardID, id)` field spelling commissioned for both slices (§5).

## 8. Verification

The planned evidence for every mandatory requirement. This table is the draft of the matrix rows;
`docs/testing/compiled-rust-e2e-matrix.md` is the trace of record and carries the same rows
verbatim. `Layer` is named precisely and is never `e2e` for an in-process test.

`Test name` is the **existing** test that observes the behaviour today, verified as
`fn <name>(` in `tests/e2e.rs` at the baseline. Exactly one requirement — LINKED-09, the
existing-gates-still-hold rule — has such tests, named semicolon-separated in its one row
(`tests/e2e.rs:2854`, `:3375`, `:37959`, `:37706`, `:1370`, `:40424`, `:40527`, `:15890`,
in row order): they prove today's gates on today's surface, and the implementation re-runs
them unchanged beside the new scope gate. Each of the eight names was verified present
exactly once as `fn <name>(` at the 2026-10-01 citation baseline. Full `cargo test --locked --test e2e
-- --list` enumeration ran 2026-09-26 in the Linux container (`kanban-gate:1.95-chrome-u501`,
image `f542f975e2dc`, host gate slot): 439 tests, each of the eight names present exactly
once — that count is the 2026-09-26 record; the web retirement (ADR-053) has since removed
the served-surface tests, so the current tree lists fewer and no fresh full enumeration is
claimed here.
Every other requirement is greenfield registry behaviour with no observing test
at the baseline, so its row is `none` and its Note says `no e2e coverage` plainly and names
the owning implementation row — `t-0dcbb1a9` (companions and claim scope per its work-package
title, `LINKED-01`..`LINKED-08`, `LINKED-10`..`LINKED-14`), `t-9eff9257` (contributions and
consumer integration per its work-package title, `LINKED-15`..`LINKED-21`), or `t-db6937ba`
(exposure and workflow proof per its work-package title, `LINKED-22`, `LINKED-24`,
`LINKED-25`; `LINKED-23` is withdrawn with the served surface and owned by no implementation
row) — which writes the fixed test set the implementation lands under. This is the precedent `docs/specs/spa.md`
set at creation (`cd55cbc`): real names where the behaviour is observable today, `none` plus the
owning row where it is not — and no invented name anywhere: every name below was verified in the
tree, and every `none` says so.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `LINKED-01` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (one stored pairing read identically from both sides) |
| `LINKED-02` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (`(boardID, id)` identity; display-name/path/token writes refused) |
| `LINKED-03` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (symmetric edges; no one-sided live exposure) |
| `LINKED-04` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (incarnation pins across rename/retire/recreate) |
| `LINKED-05` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (restart-identical reads from both sides) |
| `LINKED-06` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (unknown/denied refusals with no partial writes or leakage) |
| `LINKED-07` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (durable binding; only selected tasks claimable) |
| `LINKED-08` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (one gate across candidates, next, named, handoff, session resumption) |
| `LINKED-09` | MUST | process | `compiled_binary_allows_exactly_one_concurrent_claimer`; `claim_candidates_are_read_only_and_match_the_atomic_scheduler`; `completion_gates_apply_to_lease_taking_handoff_acceptance`; `completion_gates_track_prerequisite_lifecycle_and_cleared_dependencies`; `compiled_binary_persists_across_processes_and_rotates_handoff_lease`; `claim_next_and_candidates_skip_restricted_rows_unless_the_model_matches`; `handoff_accept_honours_the_task_model_allow_list`; `a_transacted_write_is_identical_to_the_same_write_on_its_own` | atomic ownership, read-only scheduler parity, handoff gates, lease rotation, scheduler filtering, and single-board transact identity today; all re-run unchanged beside the new scope gate |
| `LINKED-10` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (descendants and neighbours stay out until explicitly added) |
| `LINKED-11` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (revocation/claim serialization; stale-revision refusal) |
| `LINKED-12` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (append-only audited revisions; `audit verify` healthy) |
| `LINKED-13` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (frozen parks taking; revoked ends taking, keeps heartbeats) |
| `LINKED-14` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (triple-checked claims; authorized exit/rebind only) |
| `LINKED-15` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (append-only contributions with full identity; no short hashes) |
| `LINKED-16` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (exactly one of the four evidence roles) |
| `LINKED-17` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (merge/squash mapping retained) |
| `LINKED-18` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (exact consumer commit plus complete nested path) |
| `LINKED-19` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (moving latest and near-misses satisfy nothing) |
| `LINKED-20` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (no false joint marking; close needs all deliverables) |
| `LINKED-21` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (`non-code` disposition; cross-kind refusals) |
| `LINKED-22` | MUST | process | `none` | no e2e coverage — to be written by `t-db6937ba` (CLI/MCP agreement over the real stdio server) |
| `LINKED-23` | WITHDRAWN 2026-09-30 (`a-53b18f9a`) | chrome (retired) | `none` | George withdrew the served-UI obligation after ADR-053 retired its surface; no test to be written, no implementation row; ID reserved, never reused. Original wording remains above as history. |
| `LINKED-24` | MUST | process | `none` | no e2e coverage — to be written by `t-db6937ba` (mid-flight failure, save/reopen, retry-answers-stored) |
| `LINKED-25` | MUST | process | `none` | no e2e coverage — to be written by `t-db6937ba` (companion moves no gate; two-board transact refused; ordered-pair compensation) |

**Counts.** 25 IDs, never reused: 24 active requirements, all `MUST`, no `SHOULD` and no
`MAY`, plus 1 withdrawn (`LINKED-23`). By layer: 24 `process`, and the withdrawn `chrome`
row. By group: shared records 6 (`LINKED-01`..`LINKED-06`), bounded claims 8
(`LINKED-07`..`LINKED-14`), delivery evidence 7 (`LINKED-15`..`LINKED-21`), surfaces and the
sibling slice 4 (`LINKED-22`..`LINKED-25`, of which `LINKED-23` is withdrawn). LINKED-09
carries one row naming eight existing tests; the remaining 23 active requirements carry
`none` rows, each owned by `t-0dcbb1a9`, `t-9eff9257`, or `t-db6937ba` as its Note states.

**How §4 reaches every MUST.** A1 covers `LINKED-01`, `LINKED-02`, `LINKED-03`, `LINKED-05`; A2
covers `LINKED-02`, `LINKED-04`, `LINKED-06`; A3 covers `LINKED-07`, `LINKED-08`, `LINKED-10`;
A4 covers `LINKED-11`, `LINKED-12`, `LINKED-13`, `LINKED-14`; A5 covers `LINKED-15`,
`LINKED-16`, `LINKED-17`, `LINKED-18`; A6 covers `LINKED-19`, `LINKED-20`, `LINKED-21`; A7
covers `LINKED-22`, `LINKED-24` (`LINKED-23` withdrawn, reached from its §3 withdrawal note
and its §8 row instead); A8 covers `LINKED-09`, `LINKED-25`. Every active MUST is
reachable from §4 and from §8.

## 9. Change log

- `2026-09-26` — created at `DRAFT — gate requested` from `TEMPLATE.md`: 25 requirements in
  four groups — shared records (one stored relation, UUID identity, symmetric edges,
  incarnation pins, restart identity, closed refusals), bounded claims (durable binding, one
  gate, existing gates kept, no silent descendants, serialized races, audited revisions, frozen
  vs revoked, the authority-bound triple), delivery evidence (append-only contributions, four
  roles, squash mapping, nested consumer path, no near-miss proof, exact-evidence closure, the
  `non-code` disposition), and surfaces (CLI/MCP agreement, UI parity, recovery, the
  complement-not-replace rule). All six deferred decisions settled; §7 is `None`.
- `2026-09-26` — review fixes (still `DRAFT — gate requested`): LINKED-09 is one matrix row
  naming eight existing tests; resumption defined as the two existing `--session` entry points
  (`claim`, `handoff accept`); revocation stated as LINKED-14's authorized exit, freezing
  reversible only by audited UNFREEZE, revoked leases ending only via `release`; the ordered
  pair's order and compensation defined in LINKED-25 with no implementation dependency on
  `e-df626704`; the 24 `none` rows owned by `t-0dcbb1a9`/`t-9eff9257`/`t-db6937ba`. ADR-051 §§2–3
  and the matrix section move in the same change.
- `2026-09-26` — independent-review round 2/3 fixes (still `DRAFT — gate requested`): §7 heading
  restored; LINKED-02 sentence repaired; `none`-row owners regrouped to the parent's work-package
  titles (`t-0dcbb1a9` LINKED-01..08/10..14, `t-9eff9257` LINKED-15..21, `t-db6937ba` LINKED-22..25);
  `cargo test --locked --test e2e -- --list` enumerated in the Linux container (439 tests, eight
  LINKED-09 names each present once); revoked-lease release cited at `rust/lib.rs:169`.
- `2026-09-29` — baseline refreshed to `361d7e3` on `wt/t-fe137b57-land` (SPEC-READY stamp of
  2026-09-26 kept verbatim, including its `rust/lib.rs:169` which was correct in that tree).
  Citation corrections, all verified in this tree, none changing a requirement's meaning except
  where noted: `claim` 156→`rust/lib.rs:152`, `claim --candidates` 165→`rust/lib.rs:161`,
  `handoff accept` 184→`rust/lib.rs:180` (§1, LINKED-08, §2); `task add --depends-on`
  131–133→`rust/lib.rs:125`–`rust/lib.rs:127` (§1, §5); `claim` has no `--force`
  327–328→`rust/lib.rs:325`–`rust/lib.rs:326` (§1); `RELATION_KINDS`
  `rust/model.rs:458`→`rust/model.rs:378`, content unchanged (§1); revoked-lease `release`
  `rust/lib.rs:169`→`rust/lib.rs:165` (LINKED-13); `docs/PRD.md:31`→`docs/PRD.md:32`
  (prevent-concurrent-ownership goal; §1); the eight LINKED-09 `tests/e2e.rs` lines re-pointed
  to `2854`, `3375`, `37865`, `37612`, `1370`, `40330`, `40433`, `15889` in row order, each
  re-verified present exactly once as `fn <name>(` (§8); the 439-test `--list` enumeration is
  kept as the dated 2026-09-26 record with no fresh count claimed (§8). ADR numbering checked:
  no collision — the lane holds ADR-051 (this slice) through ADR-055, so no renumber. Matrix
  section placement checked: the LINKED trace sits with the other `Requirements trace` sections
  before `Watch coverage note`, merged as a pure addition.
  MATERIAL change (re-review required): `LINKED-23` WITHDRAWN — the served web UI it named was
  deleted with no successor (ADR-053, accepted 2026-09-28: `kanban serve`, `web/` SPA, `/live`,
  every HTTP route gone; `rust/serve.rs` absent from this tree; `web-ui.md`/`spa.md` withdrawn).
  The requirement was rewritten nowhere CLI-only because its entire subject was the retired
  surface; the ID stays reserved. §2 actors/in-scope, the `http`/`chrome` layer notes, A7, §6
  Accessibility/Security, §8 counts/coverage, and the matrix LINKED-23 row move in the same
  change, kept verbatim-identical with the matrix section.
- `2026-09-30` — George chose `withdraw` on `a-53b18f9a`: LINKED-23's
  proposed 2026-09-29 withdrawal is authorized, not merely inferred from
  ADR-053. The ID remains reserved; all 24 remaining MUST requirements keep
  their IDs, scope and planned evidence. Fresh independent `/quality spec`
  review is required before this revision may be stamped `SPEC-READY`.
- `2026-10-01` — merged lane `521c19e` into the isolated candidate; the
  LINKED and CLAIM trace sections both survive the matrix merge. Refreshed
  present-tense `rust/lib.rs`, `rust/model.rs` and eight `tests/e2e.rs` line
  citations against that tree. The dated 2026-09-26 enumeration (439 tests)
  and 2026-09-29 baseline remain historical; no new test count is claimed.
