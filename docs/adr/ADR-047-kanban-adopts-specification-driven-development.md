# ADR-047: Kanban adopts specification-driven development: one BRD, one spec per slice, IDs that outlive their wording

**Status:** Accepted
**Date:** 2026-09-17 (recorded 2026-09-18)
**Deciders:** George approved the rollout on 2026-09-17 — "make sure we have e2e tests, specs,
adrs, brd, prd etc" and "write epics and tasks and approve them all" — and, the same day, "use
SDD to do the UI/UX using best practices". He owns the rollout's scope. The wording of this
document is decided by codex@driver; George may supersede any clause of it.
**Supersedes:** the join-after-release gate recorded on attention `a-180f934d` (2026-09-15),
which held that documentation work joins the board only after a release boundary; this ADR
replaces it with the per-slice gates in §6 and §8. It also absorbs `t-64fb4ae7` (the
sprint-slice specification) into the rollout of §7 rather than leaving it as a free-standing
document task.

**On the number.** The planning brief for this ADR said 046. ADR-046 is already taken by the web
design system (`/Users/geoyws/work/src/kanban/docs/adr/ADR-046-the-web-ui-is-one-designed-system.md`,
2026-09-17), so this decision is 047.

## Context

Kanban has 46 ADRs, a PRD (`/Users/geoyws/work/src/kanban/docs/PRD.md`), a compiled-binary e2e
matrix (`/Users/geoyws/work/src/kanban/docs/testing/compiled-rust-e2e-matrix.md`) and, since
2026-09-17, exactly one requirement-level specification:
`/Users/geoyws/work/src/kanban/docs/specs/web-ui.md`, written under George's "use SDD to do the
UI/UX using best practices" and stamped `SPEC-READY` on 2026-09-17
(`docs/specs/web-ui.md:10`-`docs/specs/web-ui.md:12`). It has no BRD.

The portable procedure the `/quality` gate runs is
`/Users/geoyws/.agents/skills/quality/references/spec-driven-development.md`. It deliberately
fixes no formats: "Formats and commands are project-specific. Before applying the workflow, read
the project's recorded SDD rollout decision and established BRD, PRD, specification, ADR,
planning, task, test, and trace formats" and "If no rollout has been recorded, do not invent a
wholesale adoption"
(`/Users/geoyws/.agents/skills/quality/references/spec-driven-development.md:13`). That recorded
rollout decision does not exist for this repository. Without it, the second specification is free
to choose a different file layout, a different ID shape and a second trace matrix, and the
`/quality` gate has nothing project-specific to hold it to.

The WEB slice already answered most of those questions in passing, and answered them well. This
ADR does not re-decide them; it reads the conventions off that worked example, names their
files, and says which slices the rollout covers.

## Decision

Kanban adopts the discipline *Specification -> Plan -> Tasks -> Implementation -> Verification ->
Synchronized maintenance* (SDD reference line 11) with the following formats, which are
normative for every slice specified after this ADR.

### 1. The BRD is one file, `docs/BRD.md`, and there is never a second one

Business purpose, problem, outcomes, stakeholders and business boundaries live in exactly one
document at `/Users/geoyws/work/src/kanban/docs/BRD.md`, for the product as a whole. It does not
exist yet; `t-31780208` writes it. There is no per-feature, per-slice or per-epic BRD: a slice
that needs business framing cites the BRD, and a slice that changes business framing edits the
BRD in the same change (SDD reference line 155). The same singleton rule already holds for the
product requirements document, `docs/PRD.md`, and is unchanged by this ADR.

### 2. A specification is one file per bounded slice at `docs/specs/<slice>.md`

- One bounded vertical slice, one file, under `/Users/geoyws/work/src/kanban/docs/specs/`.
- The filename is the slice ID lowercased with words hyphen-separated: slice `WEB` is
  `docs/specs/web-ui.md` (`docs/specs/web-ui.md:1`, `docs/specs/web-ui.md:5`). Slice `SPA` will be
  `docs/specs/spa.md`.
- The section skeleton is the SDD reference's minimum feature content, lines 58-67, in that
  order: §1 Identity and baseline, §2 Purpose and scope, §3 Requirements, §4 Acceptance,
  §5 Contracts and data, §6 Quality and security, §7 Open questions, §8 Verification.
  `docs/specs/web-ui.md` is the worked example of all eight.
- §1 carries the identity block: slice ID, dated baseline with a commit, status, product-scope
  owner, wording decider, sources, and the trace matrix
  (`docs/specs/web-ui.md:3`-`docs/specs/web-ui.md:35`).
- The reusable template and the writing conventions live at
  `/Users/geoyws/work/src/kanban/docs/specs/README.md`, owned by `t-edae799f`. This ADR names
  that path and does not write the template; where the two disagree, this ADR wins.

### 3. Requirement IDs are `<SLICE>-<nn>` and they outlive their wording

- Form: the uppercase slice token, a hyphen, and a zero-padded two-digit number — `WEB-01`,
  `WEB-59`. A slice that passes 99 extends to three digits (`WEB-100`) without renumbering
  anything below it.
- Numbering is by **creation order**; grouping inside the document is by **topic**, so the
  numbering is not monotonic down the page. This already holds: WEB-56..WEB-59 were added by the
  2026-09-17 gate review and sit in the topical group they belong to
  (`docs/specs/web-ui.md:84`-`docs/specs/web-ui.md:86`, and the same statement in §1 at
  `docs/specs/web-ui.md:5`-`docs/specs/web-ui.md:7`).
- IDs are stable across wording refinements: a clearer sentence for the same obligation keeps its
  ID (SDD reference line 42).
- When the **meaning** changes, the old ID stays in place, marked `Superseded by <new ID>`, and
  the new requirement is appended at the end of the creation sequence. An ID is never reused for
  a different obligation and never deleted (SDD reference line 156: retain the original
  requirement ID, the authorizing owner, the reason, the replacement, the date and the affected
  evidence).

### 4. Acceptance examples live inside the specification, as prose

§4 of the slice specification holds concrete Given/When/Then examples, in plain prose, each one
naming the requirement IDs it proves — the form
`docs/specs/web-ui.md:503`-`docs/specs/web-ui.md:562` already uses ("**A1 — the hero (WEB-01,
WEB-02, WEB-04, WEB-07).** *Given* … *when* … *then* …"). Given/When/Then here is a readable
shape, not tooling: no Cucumber, no Gherkin feature files, no step definitions (SDD reference
lines 71, 180). An example that proves nothing identifiable is not an example: every scenario
carries its ID list in its own heading, and every mandatory requirement is reachable from at
least one scenario or from §8.

### 5. One trace, and it is the existing matrix

Requirement rows go into the **existing**
`/Users/geoyws/work/src/kanban/docs/testing/compiled-rust-e2e-matrix.md`, in its "Requirements
trace" section (`docs/testing/compiled-rust-e2e-matrix.md:112`-`docs/testing/compiled-rust-e2e-matrix.md:124`).
One row per requirement ID with its strength, its layer, the **existing** test name, and a note.
The layer vocabulary is the one that matrix already uses: `unit` (a `#[test]` in `rust/serve.rs`'s
`mod tests`, reading served bytes), `chrome` (a compiled-binary test driving real Chrome in
`tests/e2e.rs`), `http` (a compiled-binary HTTP exchange, no browser) —
`docs/testing/compiled-rust-e2e-matrix.md:116`-`docs/testing/compiled-rust-e2e-matrix.md:119`
and the same three definitions in the specification at
`docs/specs/web-ui.md:74`-`docs/specs/web-ui.md:80`.

A named test is real or the row is a lie: the WEB rows were enumerated with
`cargo test --locked --lib serve:: -- --list` and `cargo test --locked --test e2e -- --list`
before landing (`docs/testing/compiled-rust-e2e-matrix.md:119`-`docs/testing/compiled-rust-e2e-matrix.md:121`),
and every later slice does the same.

**A second matrix is refused.** Not a per-slice trace file, not a spreadsheet, not a table
duplicated into the specification — §8 of a specification plans evidence and links to the matrix
rather than restating it (`docs/specs/web-ui.md:34`-`docs/specs/web-ui.md:35`,
`docs/specs/web-ui.md:741`). The SDD reference requires the same: link to an existing trace
matrix rather than a duplicate (line 67), "Reference it instead of creating a second trace
ledger" (line 166).

### 6. When a specification is required, and when a delta is enough

**`/quality spec` is REQUIRED** — a new or edited `docs/specs/<slice>.md` before implementation —
for any change to product behaviour:

- a new route, verb, subcommand or field;
- changed served markup or a changed CLI contract;
- changed refusal wording (it is the product's sentence to its user — ADR-008);
- a board schema migration.

**A requirement delta plus one acceptance example in the slice's existing specification is
sufficient** for a bounded fix inside an already-specified slice: a bug fix that restores what a
requirement already says, a copy fix within an existing requirement's wording, or a test-only
change. The delta and the example land in the same change as the code, and the matrix row is
updated if its test name moved.

The authority for the lighter path is the SDD reference itself: "A trivial fix may need only a
requirement delta and acceptance example in an existing issue or spec; it does not automatically
need separate BRD, PRD, specification, and ADR files" (line 15).

**No retroactive suite.** The same reference forbids it: "Do not migrate an unrelated project,
impose a second ledger, or retroactively demand a full documentation suite" (line 13). ADRs
001-046, `docs/PRD.md` and the e2e matrix remain the record for everything already shipped.
Shipped behaviour acquires requirement IDs only when a slice next touches it, and then only for
the part the slice touches.

### 7. The rollout is two slices, and nothing else

- **(a) `WEB` — `docs/specs/web-ui.md`.** The first slice, written and stamped `SPEC-READY` on
  2026-09-17 and served on 2026-09-17/18 under George's same-day "use SDD to do the UI/UX using
  best practices". It is the *worked example* the conventions above are read from, and is not a
  retroactive documentation suite: it was written before its own revamp was implemented, and
  ADR-046 is the decision it expresses at requirement level
  (`docs/adr/ADR-046-the-web-ui-is-one-designed-system.md:60`-`docs/adr/ADR-046-the-web-ui-is-one-designed-system.md:62`).
- **(b) `SPA` — epic `e-9306a1d9`,** the React + TypeScript rebuild of `kb.geoy.ws` (George,
  2026-09-17). It is the first slice to be specified *after* this ADR, and therefore the first
  test of these conventions on an unwritten surface.

Nothing else is in the rollout. Adding a slice needs a new row under epic `e-c0852fe7`, with
George's approval, not a reading of this ADR.

### 8. `SPEC-READY` is specification readiness and nothing else

`SPEC-READY` means the requirements and acceptance are testable, the constraints and interfaces
are explicit, verification is planned, and no material open question remains (SDD reference line
99). It authorises **neither implementation, nor rollout, nor release**; "pilot evidence and
`SPEC-READY` do not authorize implementation or wider rollout" (line 13), and
`docs/specs/web-ui.md:12` says it in the stamp itself: "Product readiness is not claimed by this
stamp."

Product readiness stays what it already was: `/quality`, then `/tidy`, then the served-tier
receipt — the measured build provenance and capability gate of
[ADR-044](ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md) and
the served-version proof of
[ADR-045](ADR-045-sprints-are-proof-gated-version-boundaries.md) §4.

### 9. Priority is the board's, strength is the specification's

Priority (`P0`-`P2`) is a board field on a task row: it orders work. Strength
(`MUST`/`SHOULD`/`MAY`, BCP 14, uppercase only where those meanings are intended) is a
requirement attribute: it says how binding the obligation is. They are never conflated — a `P0`
task can carry a `MAY` requirement and a `P2` task a `MUST` one (SDD reference line 52). The
matrix carries strength, not priority
(`docs/testing/compiled-rust-e2e-matrix.md:124`, `docs/testing/compiled-rust-e2e-matrix.md:186`).

No specification invents a performance, availability, latency, retention or security commitment.
Where one would be needed, §7 records an open question with a named owner and the gate it blocks
(SDD reference line 54); `docs/specs/web-ui.md:69`-`docs/specs/web-ui.md:70` is the worked
example of the refusal, and its §6 records one measurement as an observation rather than a budget.

## Consequences

**What becomes harder.** Any product-behaviour change in `WEB` or `SPA` now costs a requirement
ID, an acceptance example and a matrix row before it can be implemented, and the four documents
move in one change when their meaning moves together (SDD reference line 155). A test rename
becomes a matrix edit. An ID that turns out to mean the wrong thing can no longer be silently
rewritten: §3 forces a visible supersession.

**The runtime rule is being scoped, in the same working step.** Rule `r-cf2b2b9f` is scoped to
`RUNTIME`: no Node, Bun, npm, pnpm, Yarn or Corepack on `hax` or `hig`, and none needed to run
the served artefact — with a **build-time exception for the `SPA` epic** `e-9306a1d9`. The main
loop amends the registry rule in this same working step. The rule's release-gate paragraph is
untouched here; `t-d6657b71` owns it.

**The tension, stated honestly.** ADR-046 §3(a) declined Tailwind partly on the no-Node ground:
"*It buys a build step the product does not have* … A Node toolchain, a generated stylesheet and
a second artifact to keep in step is a rule this product does not have, bought for one template"
(`docs/adr/ADR-046-the-web-ui-is-one-designed-system.md:119`-`docs/adr/ADR-046-the-web-ui-is-one-designed-system.md:126`).
George's `SPA` decision (2026-09-17, after measurement) accepts exactly such a build-time
toolchain. Both stand, at different times of day and for different scopes: ADR-046's runtime
argument — one binary, the served executable proven at install time — is untouched, because the
`SPA` toolchain runs on a build host and the artefact `hax`/`hig` serve is still the compiled
binary's bytes. ADR-046's *build-step* argument was a judgement about one stylesheet for one
template, and it does not decide a full client rebuild. Whoever later reads §3(a) as a standing
ban on all toolchains should read this paragraph instead.

**What a future change must update.** A new slice: `docs/specs/<slice>.md`, its rows in
`docs/testing/compiled-rust-e2e-matrix.md`, a board row under `e-c0852fe7`, and `docs/BRD.md` or
`docs/PRD.md` if the business or product truth moved. A change to the conventions themselves is a
supersession of this ADR with its own status line, not an edit to `docs/specs/README.md`.

**What is deliberately not adopted.** No Spec Kit installation (SDD reference line 183 makes it
optional and it is not evaluated here), no Cucumber runner, no requirements-management tool, no
second trace ledger, no per-feature BRD, and no claim of ISO/IEC/IEEE 29148 conformance — the
reference disclaims it at line 5 and so does this ADR.

**Pilot evidence.** `WEB` is the recorded pilot. Its evaluation evidence — requirements proven
and by what, ambiguities caught before implementation, rework, and the cost of keeping
specification, tests, trace and code in step — is owed under SDD reference lines 17-22 and 158,
and is recorded against epic `e-c0852fe7` when the `SPA` slice reaches its first gate.

## References

- Epic `e-c0852fe7` (the SDD rollout) and epic `e-9306a1d9` (the `SPA` rebuild) — the
  authoritative requirement sources; task `t-7a89c9ef` commissioned this ADR
- Attention `a-180f934d` (2026-09-15) — the join-after-release gate this ADR supersedes; task
  `t-64fb4ae7` — the sprint-slice specification it absorbs; tasks `t-31780208` (`docs/BRD.md`),
  `t-edae799f` (`docs/specs/README.md`), `t-d6657b71` (the `r-cf2b2b9f` release-gate paragraph)
- `/Users/geoyws/.agents/skills/quality/references/spec-driven-development.md` — the portable
  procedure this ADR makes project-specific (rollout requirement at line 13, trivial-fix rule at
  line 15, minimum feature content at lines 58-67, ID identity at lines 42 and 156,
  priority-vs-strength at line 52, no invented commitments at line 54, one trace ledger at line
  166)
- [ADR-046](ADR-046-the-web-ui-is-one-designed-system.md) — the first spec-driven slice's decision
- [ADR-044](ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md),
  [ADR-045](ADR-045-sprints-are-proof-gated-version-boundaries.md) — the served-tier receipt that
  `SPEC-READY` does not substitute for
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — why changed refusal
  wording is a product-behaviour change
- `docs/specs/web-ui.md` — the worked example; `docs/testing/compiled-rust-e2e-matrix.md` — the
  one trace; `docs/PRD.md` — the product record for already-shipped behaviour
