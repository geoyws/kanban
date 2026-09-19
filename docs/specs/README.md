# Specifications: how this directory is written

A specification here is the requirement-level statement of one bounded vertical slice: what the
product must observably do, under which permissions, with which failures and data rules, and how
each obligation will be proven. It is not a design document, not a task list and not a code
description. The conventions below are not decided here — they are
[ADR-047](../adr/ADR-047-kanban-adopts-specification-driven-development.md), restated where a
writer looks; where this file and that ADR disagree, the ADR wins (ADR-047 §2). The portable
procedure they make project-specific is
`/Users/geoyws/.agents/skills/quality/references/spec-driven-development.md` (the SDD reference).
`docs/specs/web-ui.md` is the worked example of every rule below (ADR-047 §7).

## When a specification is required, and when a delta is enough (ADR-047 §6)

A new or edited `docs/specs/<slice>.md` **before implementation** is REQUIRED for any change to
product behaviour:

- a new route, verb, subcommand or field;
- changed served markup or a changed CLI contract;
- changed refusal wording (it is the product's sentence to its user — ADR-008);
- a board schema migration.

**A requirement delta plus one acceptance example** in the slice's existing specification is
sufficient for a bounded fix inside an already-specified slice: a bug fix that restores what a
requirement already says, a copy fix within an existing requirement's wording, or a test-only
change. The delta and the example land in the same change as the code, and the matrix row is
updated if its test name moved.

Nothing is specified retroactively: shipped behaviour acquires requirement IDs only when a slice
next touches it, and then only for the part the slice touches (ADR-047 §6). The rollout is `WEB`
and `SPA` and nothing else; adding a slice needs a board row under epic `e-c0852fe7` with
George's approval, not a reading of the ADR (ADR-047 §7).

## The rules

- **One file per slice (ADR-047 §2).** `docs/specs/<slice>.md`, one bounded vertical slice per
  file. The filename is the slice ID lowercased with words hyphen-separated: slice `WEB` is
  `docs/specs/web-ui.md`, slice `SPA` is `docs/specs/spa.md`. The section skeleton is the SDD
  reference's minimum feature content in order — §1 Identity and baseline, §2 Purpose and scope,
  §3 Requirements, §4 Acceptance, §5 Contracts and data, §6 Quality and security, §7 Open
  questions, §8 Verification. Copy [`TEMPLATE.md`](TEMPLATE.md) and fill it.
- **Requirement IDs outlive their wording (ADR-047 §3).** Form `<SLICE>-nn`: the uppercase slice
  token, a hyphen, a zero-padded two-digit number (`WEB-01`, `WEB-59`); past 99 it extends to
  three digits without renumbering anything below. Numbering is by **creation order** while
  grouping in the document is by **topic**, so the numbering is not monotonic down the page. A
  clearer sentence for the same obligation keeps its ID. When the **meaning** changes, the ID
  stays in place and the change is visible: a dated `*Superseded YYYY-MM-DD.*` paragraph directly
  under the requirement, quoting the wording it replaces and why it moved; where the obligation
  becomes a different one, the old ID is marked `Superseded by <new ID>` and the new requirement
  is appended at the end of the creation sequence. An ID is never reused and never deleted. Live
  examples: `docs/specs/web-ui.md` WEB-02 (twice), WEB-35 and WEB-47.
- **Strength is BCP 14, and it is not priority (ADR-047 §9).** `MUST`/`SHOULD`/`MAY`, uppercase
  only where those meanings are intended, is a requirement attribute. Priority (`P0`-`P2`) is a
  board field on a task row. They are never conflated: a `P0` task can carry a `MAY` requirement
  and a `P2` task a `MUST` one. The matrix carries strength, not priority.
- **No invented commitments (ADR-047 §9).** No specification invents a performance, availability,
  latency, retention or security commitment. Where one would be needed, §7 records an open
  question with a named owner, its status and the gate it blocks. A measurement may be recorded
  as an observation, explicitly not as a budget.
- **Acceptance examples live in the specification (ADR-047 §4).** §4 holds concrete Given/When/Then
  examples in plain prose, each heading naming the requirement IDs it proves. Given/When/Then is a
  readable shape, not tooling: no Cucumber, no Gherkin feature files, no step definitions. Every
  mandatory requirement is reachable from at least one scenario or from §8.
- **One trace, and it is the existing matrix (ADR-047 §5).** Requirement rows go into
  `docs/testing/compiled-rust-e2e-matrix.md`, in one
  `## Requirements trace — docs/specs/<slice>.md <SLICE>-01..<SLICE>-nn` section per specified
  slice, with the five columns that file's convention paragraph defines. Every mandatory
  requirement has exactly **one** row; the layer is named precisely — `unit`, `chrome`, `http` or
  `process` — and never `e2e` for an in-process test. Where there is no browser evidence, the row
  says `no e2e coverage` plainly rather than implying it. A named test is real or the row is a
  lie: enumerate with `cargo test -- --list` before landing. A second matrix is refused — §8 of a
  specification plans evidence and links to the matrix rather than restating it.
- **`SPEC-READY` is specification readiness and nothing else (ADR-047 §8).** It means the
  requirements and acceptance are testable, the constraints and interfaces are explicit,
  verification is planned, and no material open question remains — the SDD reference's §1 exit
  criteria, stamped by an independent reviewer, not by the writer. It authorises neither
  implementation, nor rollout, nor release; product readiness stays `/quality`, then `/tidy`, then
  the served-tier receipt (ADR-044, ADR-045 §4).

## Running the gate

`/quality spec` reads this directory. Run it on the new or edited `docs/specs/<slice>.md` before
implementation starts; it exits `SPEC-READY` or `BLOCKED` against the criteria above. The stamp
goes in §1 with its date and the reviewer's independence noted, as `docs/specs/web-ui.md:10`-`:12`
does.
