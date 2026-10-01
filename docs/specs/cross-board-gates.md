# Specification: cross-board readiness gates (slice CROSS)

## 1. Identity and baseline

- **Slice ID:** `CROSS`; requirement IDs `CROSS-01` through `CROSS-12` are stable.
- **Baseline:** 2026-10-01, Kanban commit `b7867f66a4dc34dc1bf1744c6e1412dc7861b93d` on `origin/geoyws`; docs writer branch `wt/t-7ecf570d-spec`.
- **Status:** SPEC-READY on 2026-10-01 for the `a-b63e7b50` clear-semantics delta (`CROSS-01`, `CROSS-05`, A2, A3): independent /quality spec review by SpecReviewCrossClear on commit `871d158`, no findings. Earlier on 2026-10-01 the slice was SPEC-READY by independent /quality spec review by CrossSpecIndependentGate on requirement/acceptance revision 3B85, with adversarial security review by CrossSpecConcurrencyReview PASS on the same revision. No receipt claims implementation, product readiness, rollout, test execution or release authority.
- **Owner (product scope):** George. Approval `a-2980be50` authorizes this CROSS slice under rollout epic `e-c0852fe7` and a specification-first restart of core task `t-e87d4704`.
- **Decider (wording):** `@:geoyws/kanban/driver`, subject to George's accepted parent contract.
- **Sources:** Accepted cross-board contract `e-df626704` (2026-09-13); existing local dependency contract `t-ae8417ae`; George's 2026-10-01 decision `a-b63e7b50` (choice `scoped`: "an empty structured dependency list keeps hidden local gates, matching today's command; the waiting board's owner may explicitly clear foreign gates"); ADR-047 (SDD procedure); ADR-053 (George's accepted 2026-09-28 retirement of all web routes); `docs/specs/linked.md` §1–2 (companion links do not own readiness); baseline CLI and projection declarations in `rust/lib.rs`, local blocker logic in `rust/store.rs` (the keep-over-refuse replacement at `rust/store.rs:7293-7310` at commit `7a1a555`), and DTOs in `rust/model.rs`. The earlier source sketch `71042c6` was quarantined after review `a-2981a67b` and is not a baseline.
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. Its CROSS rows are the trace of record; §8 states verification intent, not evidence.

## 2. Purpose and scope

**Intended outcome.** A task, story or epic on one registered board can wait for a specific item on another registered board to reach `done`. Acies readiness may gate future Unum migration work, but this feature neither judges Acies readiness nor authorizes migration.

**Users / actors.** Authorized board writers declare/replace dependencies; authorized board readers inspect blockers; the owning lane claims and progresses dependent work. The source owner alone changes the prerequisite's state and evidence.

**In scope.** Structured cross-board dependency authoring, durable board/item incarnation pins, done-only inherited gates, authority and unavailable-source masking, concurrent validation, cycle refusal, and coherent CLI/generated MCP/watch projections. Existing local dependencies use the same gate semantics without changing their input grammar.

**Boundaries.** One authoritative registry/data root and its registered SQLite boards; no cross-host lookup, remote board federation or copy of source readiness into the target. The companion relation in `docs/specs/linked.md` shares only explicit `(boardID,id)` identity; it is not a dependency and owns no gate. The source board is never mutated by dependency declaration, evaluation or removal.

**Non-goals.** No new readiness state, automatic claim/reopen/lease revocation, release authorization, cross-board subscription delivery, compensating two-board write or `doctor` repair verb. The parent epic's original web-page acceptance clause predates George's ADR-053: web routes and SPA were deleted with no successor. The historical clause remains on `e-df626704`; this specification does not silently reassign it to CLI or rebuild web functionality.

## 3. Requirements

Strength keywords have BCP 14 meanings. `process` is a compiled Rust binary invoked across real process boundaries; no in-process test alone proves a requirement here.

### Authoring and identity

**CROSS-01 — Preserve local scalars and accept one strict structured set.**
Strength: MUST · Layer: process · Source: `e-df626704` AUTHORING AND IDENTITY.
Every existing `--depends-on ID` scalar remains an opaque local ID, including slash, colon, quotes and URI-like text. `--depends-on-json` supplies the *same dependency-set operation* as an array of exact string fields `boardID` and `id`; an explicitly registered target-board UUID normalizes to the existing local edge with legacy visibility/deletion/gate semantics, while a foreign UUID creates a pinned cross-board edge. On a scratch `--db` target without registered board UUID, `--depends-on-json` refuses with the fix (register/select a board); local scalars still work. `--depends-on-json`, repeatable local `--depends-on`, and `--clear-dependencies` are mutually exclusive; mixed forms, malformed shape/fields and ambiguous entries refuse before mutation. An empty JSON array is the same operation as `--clear-dependencies` (George `a-b63e7b50`, choice `scoped`): it removes every edge the caller may write and keeps every local edge the caller may not write — a hidden (unreadable) or read-only local prerequisite stays stored, stays withheld from the caller's listing and keeps gating claims and work moves, exactly as the installed local clear does (`rust/store.rs:7293-7310` at `7a1a555`, keep over refuse). The clear succeeds rather than refusing, so it never confirms that a hidden edge exists. A non-empty replacement keeps the same caller-unwritable local edges in the same way. A stored foreign edge belongs to the waiting (target) board: it is removed by an empty JSON array, `--clear-dependencies` or a replacement that omits it whenever the caller may write the dependent row, whatever the source's state or availability (see `CROSS-05`). Unknown, denied, missing and foreign-registry sources share one non-confirming unavailable refusal for unauthorized readers; no delimiter inference or name/path alias is accepted.

**CROSS-02 — Pin both source board and item incarnations.**
Strength: MUST · Layer: process · Source: `e-df626704` AUTHORING AND IDENTITY.
A foreign-board edge binds the resolved board UUID, a non-reusable registration incarnation stored both in the registry and source board file, exact item ID and durable item-creation identity (or an equally strong, audited no-reuse invariant). A legitimate restore retains the identity only when both persisted tokens match and the board audit validates; a replacement/recreation gets a new registration identity even if a name, path or file-stem UUID repeats, and an old snapshot with a mismatched token refuses rather than retargets. Rename/repoint does not change a pin. Retired/lost/recreated boards and deleted/recreated items cannot satisfy old edges; only authorized audited replacement binds a new incarnation. Restart and archival preserve pins. A JSON edge naming the registered target board normalizes to the legacy local edge without a new pin or disclosure change.

### Gate and authority

**CROSS-03 — Inherit only done-only gates.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 1.
A waiting row inherits prerequisites declared on itself and every ancestor. Only the pinned source item in `done` satisfies a gate; `cancelled` and all other states do not. An archived `done` item on an active board remains satisfied. Do not infer a board-wide or transitive readiness state through a completed prerequisite.

**CROSS-04 — Enforce the existing work-bearing paths, not administrative paths.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 2.
An unresolved gate excludes `claim --candidates` and refuses named/next claims, lease-taking handoff acceptance, work-bearing task add/move (`in_progress`, `review`, `done`), the currently gated story-advance steps, heartbeat, and checkpoint continue/done. `--force` is not a gate bypass. Administrative draft/backlog/todo/blocked/cancelled moves, notes, release, and handoff creation retain their current behavior; independent work remains available. Source reopen immediately blocks *future* work/progress without changing an already committed target status, finished history or existing lease; the holder can still release it.

**CROSS-05 — Mask unreadable source state without confirming existence.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 3.
Source board and row/tag read authority are required before a *foreign* edge can be declared; target write authority is also required. Unknown board, denied board, unreadable row/tag, missing item and foreign-registry UUID all return the same non-confirming unavailable refusal to a caller without confirmed source read authority, before the target is written. Removing a stored foreign edge — by an empty JSON array, `--clear-dependencies` or a replacement that omits it — needs only write authority over the dependent row on the waiting board (George `a-b63e7b50`): it succeeds when the source board or item is denied, missing, retired, recreated or otherwise unavailable, reads no source state, and its result, refusal and event payload carry no source title, status or confirmation that the source board/item exists. A caller who later can read the dependent row but not its source may see the dependent’s own edge identity `(boardID,id)` and declaring owner, but no source title, status or confirmation that the source board/item exists. CLI, MCP, watch metadata, diagnostics and links use the same mask. Authorized source readers may see current state and specific unavailable reason within their authority. Explicitly local edges keep existing local status/title visibility rather than changing it by input spelling; no protected value leaks via error or event payload.

**CROSS-06 — Fail closed when a pinned source is unavailable.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 3.
Missing, corrupt, retired or unreadable board data; *any* source board older than the CROSS-aware schema or registry registration version; unknown, deleted or incarnation-mismatched items; and denied source authority never count as ready. Source lookup never migrates a source database: its owner must independently upgrade before it can be a readable prerequisite. Old binaries must refuse a CROSS-aware source board schema even when invoked directly with `--db`, so a nonparticipating source writer cannot race an otherwise locked claim. The target edge remains visible to its authorized owner for explicit replace/clear; source disappearance never cascades it away. A refused work attempt changes no target row, lease, edge or audit event, and evaluation writes no source data.

### Atomicity and concurrency

**CROSS-07 — Refuse dependency and mixed ancestry cycles.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 4.
Self, transitive cross-board, reciprocal and mixed parent/dependency cycles (including A depending on B while B depends on A’s descendant) are refused using qualified board/item incarnations, not ID alone; validation covers batched dependency and parent/reparent edits. Classify an edit by the *reachable graph after the proposed edit*, not its input flag: whenever that graph crosses a stored foreign edge, even if the new edge or reparent operation is local, run the qualified cycle check. If any reached node is unreadable, fail closed with the same non-confirming unavailable refusal whether or not a hidden cycle exists. Only when the entire reachable graph is local does the legacy local cycle check and disclosure behavior apply. Fully authorized callers see a specific actual-cycle refusal. Refusal leaves all target edits/events absent, preserving deterministic blocker order and declaring-owner provenance.

**CROSS-08 — Serialize readiness and graph changes with the target write.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 5.
Gate validation and dependent claim/progress commit serialize against *every* relevant source status, archive, board retirement/replacement, registry repoint/restore, row/tag authority and graph change across real processes; those source-side operations participate in the same ordered coordination as target work. A source cannot satisfy a foreign edge until its board and registry identity schemas are CROSS-aware, making old binaries refuse writes/open even via direct `--db`; merely readable legacy identity is insufficient. Reopen committed before dependent claim commit prevents it; claim first may succeed but later work refuses. Reciprocal edits and three or more source boards acquire operation-scoped locks in stable board-ID order without deadlock, including a global registry/restore boundary where applicable. A reader with only source read authority can participate: an operation lock is not a source DB write, no writable source DB connection is needed, and evaluation never migrates/edits source data. Target edge edits/claims are atomic with no partial target edge, lease or event on refusal. Advisory candidates reads never reserve readiness; WAL snapshot or target-only `BEGIN IMMEDIATE` cannot serialize a source writer.

### Projections, audits and compatibility

**CROSS-09 — Give every surviving read surface the same qualified answer.**
Strength: MUST · Layer: process · Source: `e-df626704` SURFACES, ADR-053.
For one committed state and the same caller authority, CLI `task show`, `task list --with-relations`, full/compact/JSON context, candidates and dashboard gated counts agree on the declaring owner, qualified source board/item and current state or masked unavailable reason. Generated MCP reads and real stdio MCP use the same contract. An unqualified local blocker remains compatible. No retired web/HTTP surface is introduced.

**CROSS-10 — Qualify audited relation identities.**
Strength: MUST · Layer: process · Source: `e-df626704` SURFACES.
Audited edge events, watch relation identities and existing subscription relation predicates distinguish equal item IDs on different boards with UUID qualification, without cross-board delivery subscriptions or event fan-in. Existing repeatable `--relation depends-on:ID` keeps opaque *local* IDs. Foreign relation filters accept additive `--relation-json` as an array of exact `{ "relation": "depends-on", "boardID": "UUID", "id": "opaque ID" }` objects; a registered target UUID normalizes to the legacy local relation, while a scratch `--db` target with no board UUID refuses this JSON filter. JSON is parsed structurally, never by splitting ID on `:` or `/`. MCP exposes the same flag; mixed local/foreign filters retain within-family OR. A local `depends-on:X` filter never matches a foreign X edge. Local-only event/filter meaning stays; source changes make later reads truthful, not synthetic target events.

**CROSS-11 — Preserve old boards and refuse old clients safely.**
Strength: MUST · Layer: process · Source: `e-df626704` ACCEPTANCE B and ADR-047.
Older target boards migrate forward without losing local edges, rows, event history or audit verification. A pre-feature board with no foreign edge retains local input/output behavior; old binaries fail closed when a board or registry schema is newer than they understand. Reopen preserves foreign board/item pins and missing-source blockers. A pre-CROSS source board is *unavailable even if an old creation identity can be read* until its owner independently upgrades its board and registration schemas. Source lookup is strictly read-only and never performs that upgrade.

**CROSS-12 — Keep reads scoped and source-only.**
Strength: MUST · Layer: process · Source: `e-df626704` INVARIANTS 5.
Candidate and projection reads reuse scoped source-board results rather than opening the same source board for each candidate or scanning every board for each row. They do not mutate the source board or persist a copied readiness cache on the target. Failure or revocation at a later read is reported from then-current authority, not a prior cached grant.

## 4. Acceptance examples

### A1 (`CROSS-01`, `CROSS-03`, `CROSS-04`)

*Given* disposable registered Acies and Unum boards, a source readiness task and an inherited Unum migration gate, *when* separate compiled CLI processes list candidates and try a named claim before source `done`, *then* the dependent is excluded/refused with qualified declaring-owner identity and independent Unum work remains claimable. After Acies `done`, a new read and claim succeed without a target-side status copy; after Acies reopens, future heartbeat/checkpoint/work moves refuse without revoking the existing lease.

### A2 (`CROSS-01`, `CROSS-02`, `CROSS-06`, `CROSS-11`)

*Given* identical item IDs on two boards and local IDs containing `/`, `:`, quotes and URI-like text, *when* local scalars and JSON edges (including a registered JSON UUID naming the target board) are added, reopened, renamed/repointed, archived, board-retired/recreated, source-deleted/same-ID recreated and restored from a legitimate snapshot, *then* local inputs have identical local disclosure/deletion/gate semantics, foreign pins never rebind, archived done still satisfies, and only authorized replacement binds a recreated source. Restore succeeds for matching registry/source tokens and valid audit, but an old snapshot with a mismatched registration token refuses; a pre-CROSS source is unavailable until separately upgraded. A scratch `--db` target refuses structured JSON without changing local scalar behavior. Malformed JSON, mixed forms and denied/unknown sources leave target rows/events unchanged. Migrated-board audit verification preserves outcomes. *And given* a dependent row holding one writable local edge, one local edge to a prerequisite the caller cannot read and one foreign edge, *when* `--depends-on-json '[]'` runs and, on a second identical fixture, `--clear-dependencies` runs, *then* both exit zero with the same stored result: the writable local edge and the foreign edge are gone, the hidden local edge is still stored and withheld from the caller's dependency listing, and the dependent stays unclaimable until that hidden prerequisite is `done`.

### A3 (`CROSS-05`, `CROSS-06`, `CROSS-09`)

*Given* a target writer/reader without source board or row/tag read access, *when* they attempt foreign-edge declaration and then task/context/candidates/MCP/watch reads or work, *then* denied, missing and unregistered sources have one indistinguishable unavailable shape, source existence/title/status remain masked, and denied declaration writes no target edge. An already stored target edge identity and generic unavailable blocker still gate work. Granting source read reveals permitted values; revocation masks the next read. A local dependency edit and a local reparent whose reachable graphs cross stored foreign edges through hidden board H are refused as unavailable, not a cycle report; a fully local edit keeps its existing authorization behavior. *And when* that same target writer, still without source read, removes the stored foreign edge with `--depends-on-json '[]'` — once while the source item exists, once after it is deleted, and once after its board is retired, each on its own identical fixture — *then* each removal exits zero, the dependent becomes claimable when no other gate holds it, and the three results and their events are identical apart from event sequence numbers, ids and timestamps, carrying no source title, status or existence signal.

### A4 (`CROSS-07`, `CROSS-08`)

*Given* three registered boards and synchronized compiled binaries, *when* reciprocal edges, a local dependency edit and a local reparent each closing a cycle through preexisting foreign edges, descendant cycles, source reopen racing target claim/heartbeat/move/checkpoint, source retire/archive/repoint/restore or row/tag grant revocation racing target work, and batched edits race, *then* one serial order wins without deadlock: source changes committed first block stale work, target-first success gates later work, and refusals leave no partial edges/leases/events. A hidden-board variant of each local edit returns unavailable without revealing whether a cycle exists. A separate pre-CROSS binary invoked on source via `--db` refuses its newer schema rather than racing the operation lock. Source read-only actors participate without source DB writes; earlier candidates reads never override write-time truth.

### A5 (`CROSS-04`, `CROSS-09`, `CROSS-10`, `CROSS-12`)

*Given* a pinned cross-board gate and an otherwise identical local gate, *when* separate compiled CLI and stdio MCP processes project candidates, task detail, context, dashboard and watch relations before/during/after source completion, *then* the same committed state produces consistent qualified blockers, correct board-scoped relation identities and no source or target-side readiness-copy write. Gated story advance, task add/move, handoff acceptance, heartbeat and checkpoint follow the same verdict; administrative actions keep their old paths.

## 5. Contracts and data

- **Interface version or schema:** Local `--depends-on ID ...` and `--relation depends-on:ID` remain opaque and local. `--depends-on-json` is a mutually exclusive replacement set of exact `{ "boardID": "UUID", "id": "opaque ID" }` objects, normalized to legacy local edges for the registered target UUID. Watch/subscription `--relation-json` adds exact `{ "relation": "depends-on", "boardID": "UUID", "id": "opaque ID" }` objects to local filters without ID tokenization; local UUID normalization applies there too. Both JSON flags refuse a scratch `--db` target lacking registered board UUID. MCP derives both flags from CLI. Forward-only target-board, source-board and registry identity migrations supply durable foreign edge, board registration and item creation identities.
- **Data invariants:** A durable foreign edge identifies source board UUID, registration incarnation, exact item ID and creation incarnation; no name/path, copied status or inferred transitive readiness. One target-board transaction owns the edge and audit record. Source reads and operation-scoped locks never edit or automatically migrate source data.
- **Migration:** Forward-only target-board and registry identity migrations retain legacy edges/history and audit health; source board identity schema upgrades are owner-initiated outside dependent reads. A pre-CROSS source remains unavailable until both board and registry identity versions are CROSS-aware, so old binaries refuse direct writes. Schema versions are assigned at implementation against the then-current baseline.
- **Compatibility:** Existing local readers/inputs retain their meaning, including scratch-board local scalars and local hidden-row cycle checks. Old binaries fail closed on newer schemas. A pre-feature target has no foreign edge until declared; local edges preserve existing visibility and delete behavior.
- **Ownership:** Target board owns the waiting row, dependencies, leases and edge audit; source board owns source state/evidence and stores its registration token. Registry stores the matching non-reusable board registration token and authoritative location; legitimate recovery requires both tokens and a valid audit, recreation requires a new token. George owns material scope changes; ADR-053 owns web deletion.

## 6. Quality and security

- **Reliability:** Atomic target edits, current source authority, ordered multi-board serialization and refusal-without-partial-write are mandatory (`CROSS-06`–`CROSS-08`).
- **Accessibility:** N/A — no surviving graphical UI or markup is changed (ADR-053).
- **Privacy:** Mask protected source title/status/existence on all projections, refusals and relation metadata (`CROSS-05`).
- **Security:** Registered board UUID and source board/tag read authority are mandatory; no foreign registry resolution, force bypass, source mutation or new credential (`CROSS-01`, `CROSS-04`, `CROSS-05`).
- **Operability:** Authorized owners get a named declaring row and qualified prerequisite plus a clear way out (finish the original item or explicitly replace/clear); inaccessible sources receive a non-confirming unavailable reason (`CROSS-05`, `CROSS-06`).
- **Performance:** No invented time budget. Scoped reuse instead of a per-candidate whole-registry scan is an observable architectural constraint (`CROSS-12`); implementation gate measures the actual process flows.

## 7. Open questions

None about product scope: George’s accepted parent epic and `a-2980be50` settle the observable behavior. The implementation ADR must choose the per-board operation-lock mechanism and durable board/item creation tokens before core code; every relevant status, availability, authority and graph writer must participate, read-only source authority must suffice for lookup, and locks must not write source data. An isolated source WAL snapshot plus a target-only write lock is not serializable.

## 8. Verification

The trace of record is the matrix section for this slice. Each row currently names `none` because the new compiled-process acceptance cases are not yet implemented; there is **no e2e coverage** for CROSS. The core implementation task must add real named process tests for A1–A5, enumerate them with `cargo test --locked --test e2e -- --list`, replace each `none` and run `scripts/release-gate.sh` inside the required Linux container before product completion. `/quality spec` reviews the planned examples and contract, not imaginary executed tests.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `CROSS-01` | MUST | process | `none` | no e2e coverage; A2 malformed/mixed/local-ID cases; A2 `[]`/`--clear-dependencies` parity keeping a hidden local gate (`a-b63e7b50`) |
| `CROSS-02` | MUST | process | `none` | no e2e coverage; A2 pin/recreate/reopen cases |
| `CROSS-03` | MUST | process | `none` | no e2e coverage; A1 done/reopen/archive cases |
| `CROSS-04` | MUST | process | `none` | no e2e coverage; A1/A5 lifecycle cases |
| `CROSS-05` | MUST | process | `none` | no e2e coverage; A3 denied/absent parity; A3 foreign-edge removal without source read (`a-b63e7b50`) |
| `CROSS-06` | MUST | process | `none` | no e2e coverage; A2/A3 no-partial-write cases |
| `CROSS-07` | MUST | process | `none` | no e2e coverage; A4 reciprocal/ancestry cycles |
| `CROSS-08` | MUST | process | `none` | no e2e coverage; A4 synchronized process races |
| `CROSS-09` | MUST | process | `none` | no e2e coverage; A3/A5 CLI/MCP parity |
| `CROSS-10` | MUST | process | `none` | no e2e coverage; A5 qualified watch/event identity |
| `CROSS-11` | MUST | process | `none` | no e2e coverage; A2 older-board migration/open |
| `CROSS-12` | MUST | process | `none` | no e2e coverage; A5 read scope/source immutability |

## 9. Change log

- 2026-10-01 — `CROSS-01`–`CROSS-12` created from the accepted parent contract; DRAFT, with no implementation or acceptance evidence claimed. The historical web clause is superseded by George's accepted ADR-053, not silently dropped.
- 2026-10-01 — Independent reviewer stamped SPEC-READY after the hidden-cycle, old-binary source-writer, authority and incarnation findings were repaired; security reviewer independently accepted revision 3B85. No CROSS code or end-to-end tests exist yet.
- 2026-10-01 — George's decision `a-b63e7b50` (choice `scoped`) applied: an empty JSON array is `--clear-dependencies` and keeps caller-unwritable local gates (keep over refuse, the installed behaviour), and the waiting board's writer may remove a stored foreign edge without source read authority or any source signal. `CROSS-01`, `CROSS-05`, A2 and A3 updated; requirement IDs unchanged in meaning otherwise. Status DRAFT until an independent /quality spec re-stamp.
- 2026-10-01 — Re-stamped SPEC-READY after independent /quality spec review of the `a-b63e7b50` delta at `871d158` (SpecReviewCrossClear): no findings. No CROSS code or end-to-end tests exist yet.
