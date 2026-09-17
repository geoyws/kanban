# ADR-046: The web UI is one designed system: the question is the hero, surfaces not boxes, one motion

**Status:** Proposed
**Date:** 2026-09-17
**Deciders:** codex@driver wrote this; George owns it. He commissioned the revamp in four
sentences on 2026-09-17 — "revamp the design philosophy please for the web"; "use SDD to do the
UI/UX using best practices"; "revamp the whole web thing for kb please and do ur best"; "make
sure u have e2e tests and unit tests" — and earlier, about the surface as shipped, "it keeps
blinking" and "the toast is too fast". He ruled on the intent, not on hexes, families,
millisecond counts or class names; those are decided here and he may supersede any of them.
**Supersedes:** nothing.
[ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) (the UI is read-only but for an
allowlist of write verbs, on loopback, behind an edge it does not implement) and
[ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) (an attention item
is a decision card with authored choices, in a fixed order) stay in force unchanged. This ADR
decides how the pages those two describe are *drawn* and *worded*, and adds no verb, route or
column.

## Context

**Citation convention.** A bare `rust/...:NNN` below is a path under the repository root, read at
commit `56e6b24`. The design plan cited as *the plan* is
`/private/tmp/web-design-plan.md` (codex@driver, 2026-09-17, produced under `/frontend-design`),
reproduced verbatim as Appendix A of `docs/specs/web-ui.md`.

The site works and the site has no design. Everything that shipped between ADR-016 and ADR-045
was added one correct feature at a time, and the result is what the plan's critique of `56e6b24`
records, from screenshots taken 2026-09-17:

- **A SaaS-card kit.** Every unit is an outlined rounded box on a dark page — the card, each
  choice, the note field, the history rows, the nav — all at the same 6px radius regardless of
  hierarchy (`rust/serve.rs:4846`, `rust/serve.rs:4815`), so nothing is *the* thing.
- **Meta as a tracked-out dot chain.** `LOOK · review · asked by codex@driver-2 · waiting just
  now` sits above every heading (`rust/serve.rs:1836`) — the eyebrow tell.
- **The question reads as a log line.** 1.35rem with a `> ` glyph in front of it
  (`rust/serve.rs:4828`, `rust/serve.rs:4829`), not as a question put to a person.
- **The recommendation does not lead.** Two outlined buttons of near-equal weight, tinted by
  outcome hue against the base (`rust/serve.rs:4884`-`rust/serve.rs:4888`).
- **The keyboard shouts.** A separate line of eight `<kbd>` badges competes with the answers
  (`rust/serve.rs:1571`-`rust/serve.rs:1574`).
- **Motion is scattered.** A card slide-in, a receipt flash, a pulsing live dot, a toast fade and
  a notice slide (`rust/serve.rs:4853`-`rust/serve.rs:4865`, `rust/serve.rs:5092`-
  `rust/serve.rs:5098`) — and the projection swap re-runs the slide-in on a card that has not
  changed. That is the blinking George named.
- **The toast is unreadable.** `TOAST_LIFE = 6000` (`rust/serve.rs:4520`) on a surface whose whole
  job is to tell him the answer landed.

The job the page exists for has not changed: *answer the next question fast and unmistakably, and
know it was recorded*, for one person, alone, on an iPhone at 390 CSS px and a Mac at 1280+, who
lives in a terminal and reaches for `kb` first. Everything else the site shows is the ledger being
read (ADR-016), and the browser's only writes stay decide, undo, plan-open and subscription
pause/resume (ADR-016 §"Write scope"; allowlist at `rust/serve.rs:453`-`rust/serve.rs:459`).

So the decision needed here is not a feature. It is whether the UI has a stated design philosophy
that a test can hold it to, or whether it keeps accreting one correct box at a time.

## Decision

The served UI is **one designed system**, stated as six principles plus a token system, both
normative. `docs/specs/web-ui.md` is the requirement-level expression of this ADR: every clause
below has requirement IDs there, and every one of those IDs has planned unit and real-Chrome
evidence.

### 1. The six principles are normative

1. **The question is the hero.** It is the largest thing on the screen and the only serif
   (spec WEB-01, WEB-02, WEB-07).
2. **Surfaces, not boxes.** Two desk surfaces — `--base` for the page, `--mantle` for the answer
   panel, the side column and the drawer. Hierarchy is carried by fill, size and type. One
   hairline exists; there are no outlines except the focus ring (spec WEB-09, WEB-12, WEB-13).
3. **A colour is an outcome.** Green, red, yellow and blue mean approve, reject, defer and other
   everywhere, and nothing else on the page is coloured (spec WEB-11).
4. **One motion.** The card advance. Everything else appears (spec WEB-14, WEB-15, WEB-16).
5. **Sentences, not chains.** Meta reads as prose: no dot-joined strings, no caps labels, no
   glyph prefixes (spec WEB-03, WEB-22, WEB-23).
6. **The keyboard is quiet.** One hint line; the digits live on the buttons (spec WEB-27).

A change that breaks one of these is a change to this ADR, not a styling tweak.

### 2. The token system is normative

Catppuccin Mocha — the palette of the OMP theme George reads the ledger from — used with
hierarchy rather than as tint. These are the only colours the stylesheet may name, and they are
named once, in `:root` (spec WEB-08):

| token | value | role |
| --- | --- | --- |
| `--base` | `#1e1e2e` | the page |
| `--mantle` | `#181825` | the answer panel, the side column, the drawer — the desk surfaces |
| `--surface0` | `#313244` | fills: alternative answers, inputs, history rows, pills |
| `--surface1` | `#45475a` | hover/active fill, and the one hairline the design keeps |
| `--text` | `#cdd6f4` | headline and body |
| `--subtext` | `#a6adc8` | secondary prose: context, consequences, notes |
| `--overlay` | `#9399b2` | quiet meta sentences, timestamps, hints |
| `--green` | `#a6e3a1` | outcome: approve; status: done |
| `--red` | `#f38ba8` | outcome: reject; status: blocked; P0; refusals |
| `--yellow` | `#f9e2af` | outcome: defer; status: review |
| `--blue` | `#89b4fa` | outcome: other; status: in progress |
| `--link` | `#89b4fa` | links — the same blue, because a link never sits inside an answer |
| `--focus` | `#b4befe` | the focus ring, and nothing else |

Two radii exist: 8px on buttons, inputs and the note field; 999px on pills. There is one pill
style on every page (spec WEB-10, WEB-41).

**One correction to the plan, decided rather than deferred.** The plan gives `--overlay`
`#6c7086` and annotates it "≥ 4.5:1 on base: 4.6:1". The computed WCAG ratio of `#6c7086` on
`#1e1e2e` is 3.36:1, so the plan's hex failed the plan's own rule. **Resolved 2026-09-17 by
codex@driver** under George's standing instruction "use SDD to do the UI/UX using best
practices" (2026-09-17): the rule wins — quiet meta is still text, and text clears 4.5:1 — and
the token is Catppuccin Mocha `overlay2` `#9399b2` at 5.81:1, the lightest overlay step that
satisfies it (`overlay1` `#7f849c` reaches only 4.44:1). George may overrule by a later note, in
which case this table, `docs/specs/web-ui.md` WEB-08/WEB-48/WEB-49 and the plan's editor's note
change together. `docs/specs/web-ui.md` §7 OQ-1 records the resolution; it is no longer open.

### 3. Three alternatives, declined by name

**(a) Tailwind (George's suggestion) — declined.** Two reasons and one non-reason.

- *It buys a build step the product does not have.* The whole UI is two `&'static str`
  constants compiled into the binary (`rust/serve.rs:3747`, `rust/serve.rs:4799`) and served
  inline (`rust/serve.rs:3729`, `rust/serve.rs:3742`). `cargo build` is the toolchain; the release
  is one binary whose *served* executable is proven at install time
  (`hig_release_script_install_restarts_kanban_serve_and_proves_the_served_exe`). A Node
  toolchain, a generated stylesheet and a second artifact to keep in step is a rule this product
  does not have, bought for one template — there is one page shape (the shell) and one component
  of consequence (the card).
- *It would be a second vocabulary.* This ADR makes tokens and a small set of semantic classes
  normative and testable; utility classes in markup would put the same decisions in a second
  place, where a unit test over the served CSS cannot see them.
- *The non-reason:* not "Tailwind is bad". On a project with a bundler it would be a reasonable
  choice. This one has no bundler and does not want one.

**(b) Keep the current look and fix only the two complaints — declined.** It would have been the
cheapest change: raise `TOAST_LIFE` and stop re-animating the current card. It is declined
because the thing George rejected is the *kit*, in his words on 2026-09-17 — "revamp the design
philosophy please for the web", "revamp the whole web thing for kb please and do ur best" — and
the critique in the Context section is structural: one radius on everything, meta as a dot chain,
two equal buttons, a `<kbd>` row. Fixing the two symptoms would have left the page with no stated
design, which is the condition this ADR exists to end.

**(c) Mono everywhere — the phosphor skin — declined, and already retired.** The pre-2026-09-11
surface leaned on a terminal register: mono for labels, a `>` glyph before every heading
(`rust/serve.rs:4812`, `rust/serve.rs:4829`), neon accents. George released us from it himself
on 2026-09-11 — "you don't need to have to use the neon color thing strictly, it can feel more
like OMP's theme here" (recorded at `rust/serve.rs:4782`). It is declined for a second reason
too: mono for prose is the same generic default the plan's review pass rejects, and it would
compete with §4's one serif instead of leaving it alone. Mono survives only where the content
*is* code (spec WEB-05).

George may overrule any of the three. If he does, this ADR, the specification and the trace
matrix change in the same change (`docs/specs/web-ui.md` §7 OQ-3).

### 4. Typography: one serif, and it is spent on the question

- **Question and page titles:** `ui-serif, 'New York', 'Iowan Old Style', Charter, Georgia,
  serif`, semibold 600. A question set like a headline, in a face that reads as *written to a
  person*.
- **Everything else:** `-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif`.
- **IDs, SHAs, commands, keys:** `ui-monospace, SFMono-Regular, Menlo, monospace`, and only for
  things that are literally code (spec WEB-05).

No font is downloaded: the stack is system faces, so the page needs no external origin and the
edge's `self`-scoped policy keeps applying untouched (spec WEB-54).

**The caveat, from the plan's own review pass.** A serif display face is close to a generic
"designed" default — on cream paper it *is* the tell. It is spent here anyway, in exactly one
place, because on a Mocha-dark desk in George's system serif it is specific to his devices and to
the one idea the page is about: a question asked of a person. It is the design's single bold
move, and if it is ever spent twice this ADR has been broken.

### 5. Motion: one moment, and it never repeats itself

The **card advance** is the only non-user-triggered motion: out `translateX(-16px)` plus fade
over 140 ms, in from `+16px` over 140 ms. Nothing else animates — no hover transitions, no
receipt flash, no pulsing live dot, no toast fade. A toast appears; a history row appears
(spec WEB-14, WEB-15).

Two consequences are normative because they are the two things George complained about:

- A projection refresh **must not** re-run the advance on a card that is still the current card.
  The refresh diffs by `data-item` and replaces only nodes whose markup changed; the current
  card's node is not touched while it is unchanged (spec WEB-16).
- A toast **stays 20 s**, pauses while hovered or focused, dismisses on click or `Esc`, and at
  most three stack, newest on top (spec WEB-19).

`prefers-reduced-motion: reduce` removes the advance too (spec WEB-20).

A notice arrives in the toast strip, which is a `role=log` region; the connection line is the
one `role=status` element on the page and says only `connecting`, `live`, `reconnecting` or
`sending`. The baseline gives both regions `role=status` (`rust/serve.rs:1594`,
`rust/serve.rs:1624`), which makes a notice and a socket state the same kind of announcement;
they are separated here (spec WEB-52, WEB-59).

Feedback is fill, not grey: the pressed answer reads `Sending…` on its own fill and the *other*
answers dim; the receipt then lands with a 3px left rule in its outcome's hue (spec WEB-17,
WEB-18).

### 6. Copy: meta is a sentence, and buttons say what happens

- Meta is prose: "codex@driver asked on px, 3 days ago"; "claude@driver-2 started this 2 hours
  ago"; "decided by geoyws just now". Never a pipe, never a dot chain (spec WEB-22, WEB-23).
- Buttons say what happens: the choice label as its raiser authored it (ADR-042 §1), and the
  free-text one is "Record my answer" (spec WEB-25).
- The note field's label is "Add a note", with no hint under it — the fold's own summary explains
  the other path (spec WEB-24).
- The empty deck is an answer, not an absence: "Nothing is waiting. Every question an agent
  raised has an answer.", with "See what was decided" (spec WEB-26).
- A refusal is the board's own sentence, in `--red` on the panel, with no box (spec WEB-29).
- Counts read as counts: "3 left" on the deck bar, "12 open across 4 boards" on `/all`
  (spec WEB-28).

### 7. What this ADR does not change

The route table, the four POST verbs, every HTTP status, form field, query parameter and `/live`
frame, the board schema, and ADR-042 §5's card order — question, context, recommended,
alternatives, reply, folded body, meta — are untouched, and `docs/specs/web-ui.md` WEB-55 pins
the route table and the write allowlist so that a revamp cannot quietly grow the surface.

## Consequences

**What becomes harder.**

- **A system-font stack looks different per operating system, on purpose.** On George's devices
  the question renders in New York (macOS/iOS `ui-serif`). On Windows there is no `ui-serif`, so
  it falls to Georgia; on Linux the generic `serif` resolves to DejaVu Serif or whatever
  fontconfig prefers. The design is therefore *specific to his devices and acceptable elsewhere*
  — a deliberate trade for downloading nothing. A future reader who sees Georgia has not found a
  bug.
- **"No outlines" is a constraint, not a style.** Every new control has to earn its place with
  fill, size and type. The next person who wants a border has to change §1 clause 2 or add to the
  spec's allowlist, in the open.
- **One motion means new interactions get no animation.** The budget is spent.
- **The quiet-meta token now carries an arithmetic obligation.** Changing `--overlay`, or putting
  it on a new background, means re-running the contrast arithmetic; the spec's unit test computes
  ratios from the tokens rather than pinning numbers, so it fails on the change rather than on a
  stale constant.

**One number is above the standard on purpose.** The 44 × 44 CSS px hit target on the deck's
controls is Apple's Human Interface Guidelines figure, not WCAG's (SC 2.5.8 asks 24 × 24). It is
a product commitment under George's "best practices", recorded as such in
`docs/specs/web-ui.md` §6 so nobody later reads it as a conformance requirement.

**What the tests pin.** `docs/specs/web-ui.md` §8 plans 59 requirements' evidence: unit tests in
`rust/serve.rs`'s `mod tests` over the served CSS, markup and copy (tokens declared once, no
border or outline outside the allowlist, two radii, no dot-chain meta, one pill style, mono only
on code, the exact copy strings, contrast arithmetic over the token pairs, the route table), and
compiled-binary real-Chrome tests for everything that is only observable as behaviour (the
advance and its non-repetition, `Sending…`, the 20 s toast, focus visibility, 44 px tap targets,
no horizontal overflow at 390/820/1280, the two-column desk). Four matrix rows are added to
`docs/testing/compiled-rust-e2e-matrix.md`.

**What a future change must update.** The specification, this ADR and the trace matrix move
together. A visual change that contradicts a principle or a token is a supersession of this ADR
with its own status line — not an edit to the stylesheet. A change that only implements what is
already stated here updates the spec's evidence column and nothing else.

**Acceptance.** This ADR stays **Proposed** until George has seen screenshots of the restyled
deck on the phone and on the Mac (`docs/specs/web-ui.md` §7 OQ-4). One open question remains for
him there: OQ-2, the SC 1.4.11 reading of borderless alternatives. OQ-1 (`--overlay`'s hex) and
OQ-3 (Tailwind) are resolved here, in §2 and §3, and he may overrule either by a later note.

## References

- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) — kanban serves its own UI: loopback
  only, the edge authenticates, and the browser's writes are an allowlist
- [ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) — an attention
  item is a decision card with authored choices; §5 fixes the card's order
- [ADR-045](ADR-045-sprints-are-proof-gated-version-boundaries.md) — the served pages this ADR
  restyles include the sprint pages
- `docs/specs/web-ui.md` — the requirement-level specification of this decision, with the design
  plan verbatim as Appendix A
- `docs/testing/compiled-rust-e2e-matrix.md` — where this slice's evidence rows land
