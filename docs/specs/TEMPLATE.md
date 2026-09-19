# Specification: <one-line claim this slice makes> (slice <SLICE>)

<!-- Copy this file to docs/specs/<slice>.md. Every section below is required; delete none of
them. Read README.md for the rules and docs/specs/web-ui.md for the worked example. -->

## 1. Identity and baseline

<!-- Who may change the scope, what the document was written against, and where its facts come
from. Every "today" claim elsewhere cites its source as <path>:<line>. -->

- **Slice ID:** `<SLICE>`. Requirement IDs are `<SLICE>-01` .. `<SLICE>-nn`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `<YYYY-MM-DD>` at commit `<sha>` on branch `<branch>`.
- **Status:** `<DRAFT | BLOCKED | SPEC-READY on YYYY-MM-DD>`.
  <!-- SPEC-READY only when requirements and acceptance are testable, constraints and interfaces
  are explicit, verification is planned and no material open question remains, stamped by an
  independent reviewer against the SDD reference's §1 exit criteria. It authorises neither
  implementation nor rollout nor release; say so in the stamp. -->
- **Owner (product scope):** `<name>`. <!-- The one person who resolves scope and wording. -->
- **Decider (wording of this document):** `<name>`.
- **Sources:** <!-- Decisions, ADRs, quoted instructions, and the shipped surface at the baseline. -->
  - `<path-or-ADR>` — `<what it settles>`.
  - `<person, date>`: "`<quoted instruction>`".
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 names the rows this slice adds;
  this specification does not restate the matrix.

## 2. Purpose and scope

<!-- The outcome in one paragraph, then who it is for and exactly where the slice stops. -->

**Intended outcome.** `<the one job this slice does, in a sentence a reader can hold>`

**Users / actors.**

- `<actor>` — `<what they do here, on what device or interface>`.

**In scope.** `<the routes, verbs, commands, fields or surfaces this slice specifies>`

**Boundaries.** `<what the slice touches at its edges but does not own, and who owns it>`

**Non-goals.**

- `<a thing a reader would otherwise assume, and why it is excluded>`

## 3. Requirements

<!-- One block per requirement, in creation order within topical groups. Strength is BCP 14;
Layer is exactly one of unit / chrome / http / process; Source cites the decision or instruction
behind it. State the observable behaviour or invariant, and where they apply, the permissions,
the failure behaviour, the data rules and the quality constraints. -->

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

### `<topical group>`

**<SLICE>-01** — `<one-line title in the imperative present>`.
Strength: `<MUST | SHOULD | MAY>` · Layer: `<unit | chrome | http | process>` · Source: `<source>`.
`<The observable behaviour or invariant, stated so that a test can fail it: the exact selector,
route, field, byte, exit code or sentence.>`
`<Permissions: who may do this, and who is refused — or omit if the requirement carries none.>`
`<Failure behaviour: the exact refusal or error the product gives, and what it leaves unchanged.>`
`<Data rules: what is written, what is never written, and what survives a restart.>`
`<Quality constraints: accessibility, privacy or operability obligations that are part of this
obligation rather than a separate one.>`

<!-- Supersession, when the meaning of a requirement changes (never its ID):

*Superseded YYYY-MM-DD.* The wording this replaces read: "<old wording, verbatim>". <Why it moved,
with the instruction or measurement that moved it.>
-->

## 4. Acceptance examples

<!-- Concrete Given/When/Then in plain prose; every heading names the IDs it proves. No Cucumber,
no Gherkin files, no step definitions. Cover the successful path and, where relevant, invalid
input, unauthorised access, failure recovery, idempotency and concurrency — or say why not. -->

### A1 (`<SLICE>-01`, `<SLICE>-02`)

*Given* `<the initial state, seeded exactly>`,
*when* `<the single action>`,
*then* `<the observable outcome, in values a test reads>`.

## 5. Contracts and data

<!-- Each line is answered or marked "N/A — <reason>". Never silently omitted. -->

- **Interface version or schema:** `<the route/CLI grammar/event schema and its version>`
- **Data invariants:** `<what is always true of the stored data>`
- **Migration:** `<the migration this slice needs, and its direction>`
- **Compatibility:** `<what an older client or board still sees>`
- **Ownership:** `<who owns the data this slice reads or writes>`

## 6. Quality and security

<!-- Applicable constraints only, each observable. No fabricated commitments: where a
performance, availability, latency, retention or security number would be needed, write an open
question in §7 with its owner and the gate it blocks, not a guess. A measurement may be recorded
as an observation, explicitly not as a budget. -->

- **Reliability:** `<or N/A — reason>`
- **Accessibility:** `<or N/A — reason>`
- **Privacy:** `<or N/A — reason>`
- **Security:** `<or N/A — reason>`
- **Operability:** `<or N/A — reason>`
- **Performance:** `<an observation with its measurement, or an open question — never a budget>`

## 7. Open questions

<!-- Every material question, its owner, its status and the gate it blocks. Write `None` and mean
it when there are none. -->

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | `<the question>` | `<name>` | `<open \| answered YYYY-MM-DD>` | `<the gate>` |

## 8. Verification

<!-- The planned evidence for every in-scope mandatory requirement. This table is copied INTO the
matrix's per-slice trace section in docs/testing/compiled-rust-e2e-matrix.md: the matrix is the
trace of record, this table is its draft. Do not build a second matrix here. Layer is named
precisely and never "e2e" for an in-process test; where there is no browser evidence the Note
says `no e2e coverage` plainly. Every test name is real, enumerated with `cargo test -- --list`
before landing. -->

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `<SLICE>-01` | `<MUST>` | `<unit>` | `<exact #[test] fn name>` | `<or empty>` |

## 9. Change log

<!-- Dated supersessions. The requirement's own *Superseded YYYY-MM-DD.* paragraph in §3 keeps
the old wording; this list is the index. -->

- `<YYYY-MM-DD>` — `<SLICE>-nn` superseded: `<what changed, and who asked>`.
