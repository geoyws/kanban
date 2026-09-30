# ADR-056: Ordered operation locks serialize cross-board gate reads and writes

**Status:** Accepted 2026-10-01 by engineering decider @:geoyws/kanban/driver, within George's accepted CROSS contract e-df626704. This approves the lock design only; no product code, e2e or release is accepted.
**Date:** 2026-10-01
**Decider:** `@:geoyws/kanban/driver`, for engineering mechanics delegated by accepted epic `e-df626704`; George retains product-scope authority.
**Requirements baseline:** `docs/specs/cross-board-gates.md` at published docs commit `390b35709d2400f76773066602090b5d0b248f54`, SPEC-READY 2026-10-01.
**Related requirements:** CROSS-04–CROSS-08, CROSS-12. The identity-token choice is separate in ADR-057.
**Supersedes:** None. ADR-008's root restore lock, ADR-009's SQLite write contention and ADR-041's single-board transaction remain in force.
**Review receipt:** independent lock reviewer CrossLockADRReviewer PASS (F95A), identity reviewer CrossIdentityADRReviewer PASS (combined F95A/6656), and adversarial interaction reviewer CrossADRInteractionSecurity PASS (F95A/6656); earlier blocked interleavings were repaired before acceptance. Owner choice a-b63e7b50 still blocks CROSS code pending specification delta.

## Context

A gate on board T reads the status on board S, then commits a claim on T. SQLite WAL lets S reopen
immediately after a read snapshot, while `BEGIN IMMEDIATE` on T excludes only T writers. If S's reopen
commits first and T's claim commits against the old snapshot, CROSS-08 is false. Ordinary commands currently
take a *shared* data-root lock; only restore/adopt take it exclusively
(`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/lib.rs:5983-5991`,
`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/lock.rs:79-105`). Board writes start `BEGIN
IMMEDIATE` in `WriteScope`
(`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/store.rs:4305-4326`). Neither existing lock
serializes S and T together. Policy grant/revocation and board retirement also live in the registry, outside
T's SQLite transaction. Source lookup must not write S or demand source-DB write permission. Pre-CROSS
binaries are excluded by the source board and registry schema gates chosen for this slice (CROSS-06/08/11).

## Proposed decision

1. Keep the existing data-root lock **shared** for ordinary managed commands and **exclusive** for
   restore/adopt. Add persistent private advisory markers in the same data root: a registry-operation lock
   and per-registered-UUID board-operation markers, each with a stable writer-intent marker. Never unlink
   them while a registry may use them; guards release on unwind/crash. For each marker, a shared entrant
   briefly takes shared intent, acquires its shared operation lock, then releases intent; an exclusive
   writer takes intent exclusively *before* waiting for the exclusive operation lock and retains intent
   through commit. New readers therefore queue behind a waiting grant-revoke, reopen or other writer, while
   existing readers drain; bounded timeout refuses rather than silently allowing starvation. Source
   evaluation holds shared board operation locks and opens S only through a strict read-only, no-sweep,
   no-migration constructor after probing its schema; older/unprovable sources are unavailable untouched.
   Every board writer holds its own marker exclusively. Lock markers are not source board data and never
   require source-DB write access.
2. Order for managed operations: root lock; registry intent/operation pair; touched board intent/operation
   pairs ascending UUID; then authoritative SQLite transactions/source snapshots. No lock upgrade or
   lower-ordered reacquisition. Target board exclusive and source boards shared remain held through target
   commit. Restore/adopt take root exclusively at entry, then registry/board pairs in the same order. Locks
   are taken **once** at the outermost operation; nested Store, registry, dispatcher and MCP calls inherit a
   held-guard capability, never reacquire flock via a second fd in the same process. For direct file
   addressing, do a non-authoritative strict no-symlink probe of the opened file and derive its root R from
   a real R/boards/UUID.db path, not from the caller's environment variable. Validate link count one, fd
   (device,inode) equals the registered path, and that R has an active registry with matching estate
   UUID/path/token and no snapshot manifest.json. A token-bearing copy, alias or mismatch refuses. Managed
   mode requires R to equal the managed canonical root; direct/test mode requires R to equal the configured
   active data_root() so disposable registries are usable. Acquire locks **in R**, then recheck
   fd/path/schema/tokens/authority under them; an alternate environment cannot redirect the lock. Registered
   but not-yet-tokenized files under R also take their board marker or refuse. Token-less files outside
   active registered roots stay scratch/local-only. MCP locks per call and standalone dispatcher per
   job/write, including BoardSelector::Db.
   Compare realpaths of the configured root and R before enforcing interior-path rules: macOS /var is an
   OS-level symlink to /private/var, not a board-file alias. Reject symlink components *inside the
   normalized R*, including a symlinked board file, but do not reject a legitimate /var prefix. The pre-root
   probe may read file metadata only to derive R; after root lock acquisition the fd, inode, registration
   path/token and authority are checked again before any DML.
   Adoption is a separate helper process: the parent acquires root exclusive, passes that SAME held
   root-lock open-file-description fd to the helper with explicit inherited-fd mapping, and waits without
   holding registry/board pairs. The helper verifies the inherited fd with fstat, then itself acquires
   registry/board pairs and registers its per-connection write permit before publishing a board or registry
   row. If the parent dies, the helper-held fd retains root exclusivity until helper exit. Nested
   same-process calls inherit a guard token; cross-process adoption inherits only the root fd, not a
   fictitious process-local token.
3. Under root and registry shared locks, preflight the reachable registered UUID set without writing.
   Acquire the sorted board set and **re-read** board identity, authority, edges and source status under
   those locks. If revalidation finds another board, release *only* board markers, retaining root and
   registry guards, then reacquire the expanded sorted set and re-read everything before target `BEGIN
   IMMEDIATE`. Use the existing bounded contention deadline; fail closed with a retryable refusal if churn
   persists or a lock is unavailable. No preflight result authorizes a write. Only a fully locked recheck
   can authorize target commit; refusal leaves target rows/leases/edges/events unchanged. Purely local
   reachable graphs keep existing cycle behavior, but a local edit reaching a stored foreign edge takes the
   qualified path (CROSS-07).
4. Registry authority/availability mutations, source status/reopen/retag/remove/archive/import writers,
   target claim/move/checkpoint/heartbeat and scheduler/system paths participate at their real entry points.
   Registry guard stays through target commit; source-only writers take their board marker exclusively.
   New-binary direct file opens and dispatcher writes take the same marker or refuse. Registry writer intent
   gives a queued revoke precedence over new shared entrants; board writer intent similarly protects reopen.
   A denial discovered under board markers is **terminal** for that operation: release board markers, record
   denied-attempt audit under registry exclusive in canonical order, and return refusal without any target
   commit. Any other late-discovered registry write cancels the in-progress target write and restarts the
   whole operation from a declared registry-exclusive scope, revalidating all facts and locks from scratch
   before mutation; never resume after releasing boards on a prior gate verdict. Old binaries cannot *open*
   CROSS-aware schemas; a pre-opened old connection instead meets the atomic write fence in clause 5.
   Resolve board names and paths through strict read-only identity preflight. Registry recency stamps are
   best-effort ordering writes, not authority changes: under a **shared registry operation guard**, a
   separate connection-local recency capability permits only column-scoped UPDATE OF last_used_at on boards
   and workspace_roots. Other registry columns, INSERT/DELETE and authority writes still require the
   exclusive registry guard and the full write capability; a statement touching last_used_at plus another
   column must satisfy that stronger guard. This preserves existing recency writes without making every
   command wait on an exclusive registry section; SQLite serializes each brief recency DML as it already
   does, while unrelated board operations keep their shared registry locks. Audit initialization, embeddings
   and expired-claim sweeps are *not* recency exceptions: obtain required board/registry exclusive pairs and
   per-connection permit BEFORE a writable constructor; pure source preflight never sweeps or migrates. A
   denial under board locks is terminal, not a reason to resume after a separate registry audit.
5. **Only a pending CROSS schema step** takes the actual data-root lock exclusively for owner upgrade;
   current-version new board registration keeps the existing shared root plus init lock, while registration
   into a pre-CROSS registry performs the explicit upgrade first. Root exclusivity drains old lock-taking
   commands on that root, **not** pre-opened direct/alternate-root connections with no matching flock; those
   rely on SQLite triggers. One board or registry version-step transaction atomically backfills tokens,
   creates guard triggers, bumps user_version past pre-CROSS support and commits; no token-bearing version
   commits before its full fence. A previously writing old transaction serializes before migration; a
   pre-opened old connection writing after commit sees the new triggers and refuses. Board/registry triggers
   call connection-local cross_write_guard(); old binaries/bare SQLite lack it. New binary functions succeed
   only with held verified operation guards for that connection (or token-less scratch local scope).
   Migrator holds its own guard during backfill. Audit trigger coverage for new tables; fence supplements
   new-binary scoped locks.
   Fence EVERY mutable application table in board and registry databases, including target
   claims/checkpoints/handoffs, task status/dependencies, audit rows and registry policy/identity. Registry
   recency is the sole column-scoped variant: UPDATE OF last_used_at on boards/workspace_roots uses
   cross_recency_guard() under registry shared, whereas UPDATE OF any other column plus INSERT/DELETE uses
   cross_write_guard() under registry exclusive; combined-column writes must satisfy both and therefore
   exclusive. Install every trigger and the user_version bump inside the same transaction. Ordinary opens of
   pre-CROSS board/registry schemas remain usable for **local-only** operations and may apply older
   pre-CROSS migrations, but MUST NOT auto-run the CROSS step under root shared; foreign
   declaration/source/target use requires both CROSS-aware schemas. Strict foreign source lookup still
   refuses all old schemas. The existing explicit kanban init on an already registered workspace/name is the
   owner upgrade boundary: read-only detect pending CROSS versions BEFORE root lock, take root EXCLUSIVE
   only if pending, re-probe and upgrade registry plus addressed board under ordered guards. Other init
   calls in a current registry retain shared root and init lock. Long-lived shared holders make a pending
   upgrade refuse honestly; never upgrade a held shared lock inside Registry::open or Store::open.
   The non-recency exclusive UPDATE triggers on boards and workspace_roots enumerate every column reported
   by PRAGMA table_info except last_used_at. Migration/upgrade verification compares the trigger column list
   to that schema list, so adding a future column without its exclusive fence is a failure. A combined
   recency+other-column UPDATE fires both guards and requires exclusive authority. A recency-only statement
   under registry shared still cannot change board identity, grants or registration.
   This CROSS-step auto-migration prohibition applies to **registered** or token-bearing board files, never
   to verified token-less unregistered scratch databases *outside* an active registry root. A fresh scratch
   file (version 0) or legacy scratch file may complete the full board migration ladder on ordinary direct
   open, including the CROSS schema step that creates all application-table DML triggers and bumps
   user_version atomically, but it mints NO registration token and performs no registry mutation. Its
   connection receives a scratch-local write capability only after validating the opened file remains
   token-less, unregistered and outside the active root; it cannot declare, serve or satisfy a foreign edge.
   Scratch SQLite BEGIN IMMEDIATE remains its local writer serialization. A token-less registered board
   inside a root is NOT scratch and stays at its old schema until owner init upgrades it under root
   exclusivity.
   Define scratch by the realpath-derived R check, not only the caller's configured data_root(): token-less
   file is scratch only when its realpath is NOT an R/boards/UUID.db under any R containing registry.db. If
   that apparent registry exists but is unreadable, or the file is orphaned/unregistered within such R,
   refuse. The scratch-capability branch of each board DML trigger reads board_meta token absence **inside
   that same SQLite write transaction** before allowing cross_scratch_write_guard(); it cannot rely on a
   cached open-time token-less classification. Registered files with missing token receive no scratch
   permit. This read is in SQL trigger logic, not a nested SQLite query from a scalar-function callback.
   Scratch classification ALSO requires fstat(opened board fd).st_nlink == 1 before inline migration and
   permit issuance; a hardlink to a registered token-less v37 board cannot become scratch via out-of-root
   alias. Link count greater than one or unreadable refuses without mutating the inode. Owner init treats a
   registered board at CROSS user_version with an **absent** file token or incomplete guard triggers as
   pending repair: root exclusive, verify board audit/path/inode, bind the existing registry token only if
   file token is absent, or reinstall missing guards only if the existing file token already matches. A
   **present but different** registration token ALWAYS refuses and is never overwritten; recovery is
   explicit replacement/re-registration with a fresh incarnation, leaving old pins unresolved (ADR-057).

## Alternatives considered

- **Target `BEGIN IMMEDIATE` plus source WAL snapshot:** rejected; S writer is not blocked. A second source
  read in the same snapshot remains stale.
- **Source `BEGIN IMMEDIATE`:** rejected; it needs a writable source connection and excludes read-only
  source access, and it does not cover registry policy or retirement. A transaction lock request alone is
  not a durable source data write, but it is the wrong permission and coordination mechanism here.
- **Root lock exclusive for all cross-board work:** rejected; it serializes unrelated boards and turns a
  scoped gate into a global bottleneck. Root exclusivity remains only for whole-root replacement.
- **Lock only the input flag's boards:** rejected; a local edit can traverse an existing foreign edge, and a
  cycle can close across three boards. Lock the reachable graph and retry after revalidation instead.

## Consequences and verification

The protocol changes all relevant writer entry points; a missing one is correctness failure. Registry
authority writes briefly exclude unrelated board writes while unrelated operations retain concurrent shared
registry/root locks. Writer-intent pairs prevent new readers starving queued revoke/reopen; existing long
readers may still cause an explicit bounded refusal. Private stable inodes, canonical file identity and
no-symlink/hardlink checks matter as much as order. Preliminary file probes are never authority; revalidate
under locks. A crash releases flock and SQLite rolls back. Nested operations inherit the outer guard rather
than self-conflicting. Every migrated board/registry write passes a schema fence before DML; manual/legacy
SQL without guarded binary refuses.
The SQLite trigger fence excludes stale cooperative binaries and guard-less DML; it does not resist a file
owner deliberately dropping triggers or altering SQLite schema by DDL. DDL tampering is outside this
design's threat model, and audit/restore procedures remain separate.
   Implementation inventory for this guard includes current writable constructors: registry rule
   consolidation calls Store::open and sweeps claims
   (/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/registry.rs:3182); canonical_rule_task_tags
   opens a board writable merely to read tags
   (/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/registry.rs:3090 — switch to readonly);
   search rebuild opens boards writable
   (/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/lib.rs:4018); restore appends a post-restore
   board event (/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/lib.rs:5111); dispatcher holds a
   long-lived Store (/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/dispatcher.rs:621). Each
   must acquire ordered pairs and per-connection permit before construction or use a strict read-only
   constructor; none is exempt from the all-table fence.

Compiled-process tests must synchronize source reopen versus target claim/heartbeat/move/checkpoint; grant
revoke/retire/repoint/restore versus target work; three-board cycles; old pre-open writers on SOURCE and
TARGET refused after atomic token-and-trigger migration; direct managed-file
alternate-root/hardlink/symlink/copy/snapshot refusal and disposable-root acceptance; dispatcher source
writes; strict read-only source lookup; graph expansion and terminal denial audit; sustained readers behind
queued writers; nested restore/MCP/dispatcher without self-reacquire; and unrelated board progress. Also
test concurrent recency stamps under shared registry scope during a long gate read, combined-column UPDATE
requiring exclusive, local-only use of v37/v14 without silent CROSS migration, existing-board init upgrading
pending schema under root exclusivity, and current-version new-board init staying shared. Assert every
mutable application table has INSERT/UPDATE/DELETE fences with the recency column exception. A Linux
throwaway SQLite 3.40.1 probe on image
sha256:f542f975e2dc78da4f9258905084bd9c7f380a656a52816f95f697628eb15cd1 observed old pre-open UPDATE refused
after a guard trigger, new guarded UPDATE succeeded. It proves only SQLite trigger semantics; named Rust
process tests and the release gate have not run for CROSS.
   Include a migration assertion that the exclusive UPDATE trigger column set equals PRAGMA table_info minus
   last_used_at for boards and workspace_roots, plus an old/new-binary direct scratch test that token-less
   branch checks registration absence inside the same write transaction. Tests cannot substitute a
   source-code grep for these runtime checks.
   Process cases must hardlink a token-less registered v37 board to an out-of-root scratch path and assert
   refusal with schema unchanged; prove owner init repairs a registered CROSS-version board with ABSENT
   token under root exclusive; then replace the board file with a different registration token, run owner
   init and assert it refuses without rewriting that token or satisfying an old foreign pin.
   Scratch process cases must include both fresh token-less /tmp/scratch.db creation and v37 scratch
   reopen/migration through the fenced CROSS step without any registry token; local scalar operations
   continue, while foreign JSON is refused. A registered v37 board in the same run must remain pre-CROSS and
   locally usable rather than silently following the scratch path.
   Additional compiled-process checks must exercise a paused old writer on the TARGET claiming across a
   newly declared foreign edge; every application table's three fence triggers in board and registry schema
   after upgrade; owner init upgrading one legacy board while a long-lived root shared holder causes an
   honest refusal; adoption helper receiving the exact root fd and publishing with its own registry/board
   guards; recency and sweep writes happening only after capabilities are held; and direct disposable root
   paths spelled /var versus /private/var on macOS (a Linux-only check cannot prove that prefix behavior).
   No runtime tests are claimed by this ADR.

## References

- CROSS specification §3 CROSS-04–08/12 and §4 A1/A3/A4 in `docs/specs/cross-board-gates.md`.
- ADR-008, ADR-009, ADR-041; core task `t-e87d4704`; accepted parent `e-df626704`.
- Source lock and write entry points:
  `/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/lock.rs:23-105`;
  `/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/store.rs:4305-4326`;
  `/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/db.rs:2945-2946`.
