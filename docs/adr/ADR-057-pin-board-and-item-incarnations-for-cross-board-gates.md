# ADR-057: Pin board registration and item creation incarnations on foreign gates

**Status:** Accepted 2026-10-01 by engineering decider @:geoyws/kanban/driver, within George's accepted CROSS contract e-df626704. This approves identity mechanics only; no product code, e2e or release is accepted.
**Date:** 2026-10-01
**Decider:** `@:geoyws/kanban/driver`, for engineering mechanics delegated by accepted epic `e-df626704`; George retains product-scope authority.
**Requirements baseline:** `docs/specs/cross-board-gates.md` at published docs commit `390b35709d2400f76773066602090b5d0b248f54`, SPEC-READY 2026-10-01.
**Related requirements:** CROSS-01–03, CROSS-05/06, CROSS-08, CROSS-11. Lock ordering is a separate choice in ADR-056.
**Supersedes:** None. Existing board file-stem UUIDs, local dependency IDs, task audit and rule-transfer provenance keep their current meaning.
**Review receipt:** independent identity reviewer CrossIdentityADRReviewer PASS (6656), lock reviewer CrossLockADRReviewer PASS (paired F95A), and adversarial interaction reviewer CrossADRInteractionSecurity PASS (6656/F95A). A copied board's present mismatched token ALWAYS refuses; earlier overbroad repair was rejected before acceptance. Owner choice a-b63e7b50 still blocks CROSS code pending specification delta.

## Context

A board's current UUID is derived from its database filename, not from a durable row inside the file
(`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/model.rs:15-22`). The registry keys boards by
path and has no registration-incarnation column
(`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/db.rs:2788-2793`). A file replaced under the
same path or a deleted item ID reused must not make a previous gate appear satisfied by a new source. Task
creation already has an audited reuse guard on current paths
(`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/store.rs:1231-1263`), but a foreign edge needs
a cheap durable identity at every later lookup, including after migration and recovery. Source status and
audit head change during legitimate work and cannot serve as the identity token. Today's baseline is board
schema 37 and registry schema 14
(`/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/db.rs:2945-2946`). Old binaries would ignore
new operation locks, so usable foreign sources must be on schemas they refuse even when opened directly with
`--db`.

## Proposed decision

1. Add a randomly minted, immutable **registration-incarnation UUID** for each registered board, stored in
   both registry row and board metadata alongside its file-stem `boardID`. Foreign lookup and *any* open of
   a token-bearing file, including direct `--db` with an alternate `KANBAN_DATA_DIR`, require the exact
   registry UUID, board UUID, registration token and file-internal matching token; the opened file must be
   the registered canonical file, not an unregistered copy or symlink alias (ADR-056). Validate active
   registration and audit integrity before reporting a detailed source cause. A caller without source
   board/row read authority receives the *same* non-confirming unavailable result for missing, denied,
   retired, unreadable, corrupt, schema-behind, foreign-registry and token-mismatched sources; no
   retired-board diagnostic oracle is reused. Rename/repoint keeps identity. Retirement is unavailable while
   retired; unretirement of the same intact registration may resume. A recreated/replaced board gets a new
   token even if name/path/file-stem UUID repeats.
2. Add immutable **item-incarnation UUID** to each task/story/epic row. Assign once to existing rows in a
   forward-only board migration and at later creation/import; preserve it across normal status, tag,
   archival, title or parent changes. Keep the existing ID-reuse audit guard. A removal and recreation
   receives a different token; an `import --reconcile` that actually replaces a live row’s content, type or
   creation identity also rotates its item token in the same audited transaction, invalidating prior foreign
   pins; a true no-op preserves it. Foreign target edges store resolved source registry UUID, board UUID,
   registration token, opaque item ID and item token. Declaration/replacement requires target write and
   source read authority; *all* unauthorized unavailable causes in clause 1 share one refusal. Edge
   removal/replacement and its audit are target-only. JSON naming registered target board normalizes to a
   legacy local edge; local `--depends-on` and scratch `--db` scalar behavior does not change.
3. Registry migration mints each registration UUID **once** for existing rows and retains it on rerun; it
   never migrates a source board merely to answer a gate. Owner-initiated upgrade first takes the **actual**
   data-root lock exclusively (ADR-056), draining old well-behaved processes; reads the registry's
   already-minted UUID under ordered guards; binds that same value into the board and assigns item tokens
   once, never minting a second board token. New registration/adoption mints once in the registry
   publication transaction and places the identical token in the staged board before publish; adoption
   unconditionally overwrites a copied registration token and retains copied item tokens only under its new
   composite board identity. If a crash leaves registry committed but board incomplete, rerun registry
   migration as a no-op and rerun owner board upgrade binding the existing token; source remains unavailable
   until both schemas/tokens match. Dependent reads use stored_schema_version and strict
   read-only/no-sweep/no-migration openers, never open_for_read_as_caller, so old/newer/corrupt sources
   remain unavailable untouched. Board and registry schema versions are both bumped past pre-CROSS binaries;
   their migration installs ADR-056's guarded DML triggers **last**, so even an old connection opened before
   the upgrade cannot later mutate a usable source or its registry policy. New connections may write only
   with the connection-local operation-guard capability; version numbers come from the implementation
   ladder.
   Root exclusivity is required only when that registry or registered board has a pending CROSS step. A
   current-version new registration retains shared root plus the existing init lock. No ordinary open runs
   the registered CROSS step, but legacy local-only use at the old schema is not refused (ADR-056). This
   qualifier applies to the word owner-initiated in the paragraph above, not to scratch.
   The owner uses the existing kanban init command against its already registered workspace/name as the
   explicit CROSS upgrade boundary; ordinary Registry::open/Store::open must refuse a pending CROSS step
   rather than auto-migrate under root shared. Init performs read-only version detection before it acquires
   root EXCLUSIVE, revalidates after acquisition, and serializes registry token mint plus the addressed
   board bind. Board and registry each place their token backfill, DML triggers on **every application
   table** (including target claims, audit and registry recency), and the user_version bump in the SAME
   version-step transaction, triggers last before that transaction commits. A committed token-bearing schema
   without full fence coverage is invalid. An old process opened before upgrade, even against a different
   lock root, then cannot mutate after either schema commit. Long-lived shared holders must stop before the
   exclusive upgrade; a refusal leaves the source unavailable until the owner retries. New registrations
   initialize under this explicit init boundary; adoption uses the root fd handed down by its parent and
   publishes with its own registry/board guard capability (ADR-056).
   Refusing a pending CROSS *step* is not refusing the whole pre-CROSS board: registry v14 and board v37
   remain usable for their existing local-only work until explicitly upgraded, with no foreign edge
   declaration/source/target allowed there. An already-current registry can create a new CROSS-aware board
   through ordinary init under the existing shared root/init lock; root exclusivity is reserved for a
   pending CROSS registry or existing-board migration. The registry write fence distinguishes shared-guard
   recency-only UPDATE OF last_used_at on boards/workspace_roots from exclusive-guard identity/authority
   changes; it must not turn every local read into a global-exclusive phase (ADR-056). Existing lookup never
   upgrades a source board as a side effect.
   Unregistered token-less scratch databases outside active registry roots are different: an ordinary direct
   open MAY migrate them all the way through the CROSS board-schema step (or create a fresh scratch board at
   that version), atomically installing all DML fences and user_version, but without a registry migration,
   board registration token or foreign edge. The validated scratch-local connection capability permits
   existing local-only writes; it cannot be reused after a file becomes registered/token-bearing. A
   token-less file already registered inside a root is *not* scratch and waits for owner init like every
   other registered source (ADR-056).
   Classify scratch from the actual file's realpath: it is not under ANY apparent R/boards/UUID.db root
   containing registry.db; an unreadable apparent registry or orphan board path refuses rather than becoming
   scratch. Each scratch write's guard trigger checks registration-token absence from board metadata INSIDE
   that write's SQLite transaction before accepting the scratch-local capability; an open-time cached
   token-less flag is insufficient. A file whose token state cannot be read is unavailable, not token-less.
   This is a trigger-side SQL read, not a reentrant same-connection query in the scalar function (ADR-056).
   Scratch candidate fd link count must equal one; hardlink alias of registered token-less v37 board outside
   R refuses before inline migration. Registered board at CROSS version with **absent** file token or
   incomplete fences with a **matching** token is pending repair: owner init takes root exclusive, verifies
   registration path/inode and audit, and may bind the already minted registry token ONLY into an
   absent-token file; a matching-token file may reinstall missing guards atomically. A **present but
   different** file registration token is a different incarnation, NEVER repaired by overwriting it, even if
   its audit verifies or item IDs/tokens happen to match; refuse until explicit audited
   replacement/re-registration mints a NEW registry token and leaves previous foreign pins unresolved.
   User_version alone never makes a foreign source ready. Repair binds token and reinstalls any guards in
   ONE transaction, never committing a token-bearing board without complete fences.
4. Legitimate restore of the **same** registered board preserves registration and item tokens only when
   registry/file tokens match and restored audit verifies. A full-root snapshot restores registry and its
   board files together under ADR-056 root exclusivity. An older or replaced snapshot without the matching
   token fails closed; gate lookup never mints a token. Adoption is a *new* registration, not an alias, even
   though copied item tokens may remain within its new composite board identity. If a board/item pin is
   missing or mismatched, keep the target edge as an unresolved blocker until authorized explicit
   replace/clear. Restored source status is evaluated afresh; never copy source `done` into target state.

## Alternatives considered

- **Board UUID/file stem alone:** rejected; a replacement file at the same path can masquerade as the old
  registration, especially after restore or manual reconstruction.
- **Item ID plus audit history/no-reuse only:** rejected as the sole lookup proof. Existing guards are
  valuable, but every foreign read would need an event-history inference and historical imported data may
  have an ambiguous first creation. A persisted per-row token makes the proof direct, while audit verifies
  its history.
- **Hash source row contents or pin audit head:** rejected; normal completion/reopen changes those bytes and
  must not retarget an otherwise unchanged edge.
- **Store a source path, name or copied status on the target:** rejected; root repoint/name reuse would
  retarget or stale-cache the prerequisite and violate source ownership.
- **Migrate the source during target lookup:** rejected; evaluation would write another board, require
  source write permission, let old binaries continue writing older sources during a claim, and break
  target-only atomicity.

## Consequences and verification

Registry and board migrations backfill and preserve tokens on rerun. Registry mint-once precedes owner board
binding under a root-exclusive upgrade; half-upgraded sources remain unavailable until deliberate rerun.
Board and registry write-fence triggers prevent pre-opened old/alternate-root connections from committing a
source or policy change after the schema becomes CROSS-aware (ADR-056). Adoption overwrites the copied
registration token and retains item tokens only under the new board identity; reconcile import rotates an
item token on actual replacement. Board create/restore, task create/import/remove, actual-root direct-file
classification and strict source read-only lookup need reviewed paths; missing/corrupt/retired sources
cannot be distinct oracles to unauthorized callers. Scratch token-less boards still use local IDs but no
structured foreign gate. Target stores identity, not status; no new permission or readiness waiver.

Compiled-binary process tests compare registry/file token matches and mismatches,
retired/re-registered/corrupt sources under authorized and unauthorized callers, same-ID
recreation/reconcile replacement, adoption token overwrite with item-token retention, legitimate versus
mismatched restore, interrupted registry-mint/board-bind migration then retry, and source lookup leaving
schema/WAL unchanged. Pause an old binary **after opening** a v37 source or v14 registry but before DML,
upgrade under root exclusivity, and prove schema-fence triggers refuse its later write; also cover a new
direct token-bearing file opened under a misleading root and a disposable registered root that remains
usable. No CROSS e2e evidence exists yet; §8 trace rows remain none until real Linux-gated tests run.
   The process acceptance must also prove that ordinary opens of pre-CROSS schema do not auto-run CROSS
   steps, that an existing-board init under root exclusive advances one addressed board and refuses while a
   shared root holder lives, and that no token-bearing user_version becomes visible between backfill and
   all-table trigger installation. This is a planned check, not executed CROSS coverage.
   Test a fresh and legacy token-less scratch board created/opened without a registry: full local operation
   still works after its inline board migration, CROSS JSON foreign input refuses, and no registration token
   is minted; contrast a registered v37 board whose local operations work but whose foreign gates refuse
   until explicit owner init.
   Add compiled-process hardlink alias of registered v37 board opened as scratch and assert refusal before
   schema change. Create a registered CROSS-version board with ABSENT token and show owner init binds
   existing registry token under root exclusive with all guards in one commit. Then copy a DIFFERENT
   token-bearing board file into the registered path, run owner init and assert it refuses without
   overwriting the present mismatched token or satisfying a target edge, even when the copied item ID/token
   and audit are otherwise valid.
   Verify one long shared-root holder blocks a pending owner upgrade honestly while a current-schema init
   still registers a new board under shared root; verify local-only actions on v37/v14 remain usable without
   declaring or satisfying foreign edges. Source/target CROSS use stays unavailable until both versions and
   tokens are current.

## References

- CROSS-01–03/05/06/08/11 and acceptance A1–A4 in `docs/specs/cross-board-gates.md`.
- ADR-032 (workspace adoption), ADR-029 (audit chain), ADR-056 (operation locks); core task `t-e87d4704` and
  accepted parent `e-df626704`.
- Existing registration and reuse code:
  `/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/registry.rs:2170-2183`;
  `/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/store.rs:1231-1263`;
  `/Users/geoyws/work/wt/kanban-t-e87d4704-adr-w1-c54bd7/rust/db.rs:3386-3398`.
