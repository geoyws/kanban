# Specification: the Kanban web UI is one designed system (slice WEB)

## 1. Identity and baseline

- **Slice ID:** `WEB`. Requirement IDs are `WEB-01` .. `WEB-59` and are stable across wording
  refinements (WEB-56..59 were added by the 2026-09-17 SPEC-READY review; numbering is by
  creation, grouping is by topic).
- **Baseline:** 2026-09-17 at commit `56e6b24` on branch `docs/t-b349fa14-spec`. Every "today"
  claim below cites the line that has it, as `<path>:<line>`.
- **Status:** `SPEC-READY` on 2026-09-17 (independent gate by a reviewer applying the SDD §1
  exit criteria; OQ-2 and OQ-4 in §7 gate ADR-046's acceptance, not this specification's
  testability). Product readiness is not claimed by this stamp.
- **Owner (product scope):** George. He alone resolves scope, colour, wording and whether a
  declined option is reinstated.
- **Decider (wording of this document and of ADR-046):** codex@driver.
- **Sources:**
  - `/private/tmp/web-design-plan.md` — the design plan (codex@driver, 2026-09-17, under
    `/frontend-design`), reproduced verbatim as Appendix A. It is the source of every visual
    decision in this specification; nothing here redesigns it.
  - George, 2026-09-17: "revamp the design philosophy please for the web"; "use SDD to do the
    UI/UX using best practices"; "revamp the whole web thing for kb please and do ur best";
    "make sure u have e2e tests and unit tests".
  - `/Users/geoyws/work/src/kanban/docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:82`
    — the browser's write scope.
  - `/Users/geoyws/work/src/kanban/docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md:71`
    — an attention item is a decision card.
  - `/Users/geoyws/work/src/kanban/docs/adr/ADR-046-the-web-ui-is-one-designed-system.md` — the
    decision this specification implements.
  - `/Users/geoyws/work/src/kanban/docs/PRD.md:239` — "The decision room" product behaviour.
  - Shipped surface at the baseline: `rust/serve.rs:394`-`rust/serve.rs:443` (read routes),
    `rust/serve.rs:445`-`rust/serve.rs:464` (the four POST verbs), `rust/serve.rs:1590`
    (`needs_you`), `rust/serve.rs:1827` (`decision_card`), `rust/serve.rs:3725` (`shell`),
    `rust/serve.rs:3747` (`JS`), `rust/serve.rs:4799` (`CSS`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 names the rows this slice adds
  to it; this specification does not restate the matrix.

## 2. Purpose and scope

**Intended outcome.** The served UI answers one job — *answer the next question fast and
unmistakably, and know it was recorded* (plan §Subject, audience, job) — and it does so as one
designed system rather than a kit of outlined boxes. The question becomes the hero; surfaces
replace boxes; a colour means an outcome; one motion exists; meta reads as sentences; the
keyboard is quiet (plan §Principles).

**Users / actors.**

- George, alone, signed in through the Google SSO at the edge, on an iPhone (Safari, 390 CSS px
  wide) and on a Mac (Chrome/Safari, 1280+). He is the only writer.
- Agent lanes, which never open the browser: they raise and read through the CLI and MCP.
- The nginx edge, which authenticates and sets the actor header
  (`rust/serve.rs:757`); kanban implements no authentication (ADR-016:49).

**In scope.** Every page `render` serves at the baseline — `/`, `/all`, `/decided`, `/boards`,
`/sprints`, `/sprints/<board>`, `/sprint/<board>/<id>`, `/plans`, `/deployments`,
`/subscriptions`, `/lanes`, `/search`, `/preview/...`, `/board/<board>`, `/task/<board>/<id>`,
`/deployment/<board>/<id>` and the not-found page (`rust/serve.rs:402`-`rust/serve.rs:442`) —
plus the shared shell, its navigation drawer, the inline stylesheet and the inline script.

**Non-goals.**

- No new write verb. The browser's writes stay exactly decide, undo, plan-open and subscription
  pause/resume (ADR-016:82, allowlist at `rust/serve.rs:453`-`rust/serve.rs:459`).
- No new route, and no change to any HTTP status, form field, header or WebSocket frame.
- No Tailwind, no Node toolchain, no build step (§7 records the decision and its owner).
- No external asset: no webfont, no image host, no CDN. The stylesheet and script stay inline in
  the served document (`rust/serve.rs:3729`, `rust/serve.rs:3742`).
- No change to the ADR-042 §5 card order: question, context, recommended, alternatives, reply,
  folded body, meta.
- No performance, latency, availability or payload-size commitment. §6 records one measurement as
  an observation, not a budget.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — a Rust `#[test]` in `rust/serve.rs`'s `mod tests` over the CSS, markup and copy
  functions and their constants. It reads served bytes, not a browser.
- `http` — a compiled-binary HTTP exchange against `kanban serve`, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome (ADR-045's web evidence
  layer), reading computed styles, geometry and behaviour.

A served invariant that is also observable in a browser is stated twice, once per layer, so that
each requirement keeps exactly one layer and George's "both unit tests and e2e tests" holds for
every invariant that has both. IDs are assigned in order of creation and never reused, while the
groups below are topical, so the numbering is not monotonic: WEB-56 .. WEB-59 were added by the
2026-09-17 gate review and sit in the group they belong to.

### Hero and type

**WEB-01** — one serif, and it is the question.
Strength: MUST · Layer: unit · Source: plan §Type, §Principles 1.
`CSS` declares `--serif:ui-serif,'New York','Iowan Old Style',Charter,Georgia,serif` and names
`var(--serif)` on exactly two selectors: the card question (`.item>h2`) and the page title (`h1`).
No third selector in `CSS` contains `var(--serif)`.

**WEB-02** — the question is set as a headline that leaves room for the answers.
Strength: MUST · Layer: chrome · Source: plan §Type scale; George, 2026-09-18 (the served deck
at 390, 820 and 1280).
`getComputedStyle` of the current card's `h2` reports `font-weight: 600`, a `font-family` string
equal to the `--serif` stack, and one `font-size` declaration at every width —
`clamp(1.375rem, 1.1rem + 1vw, 1.75rem)` — with `line-height` ≤ `1.15 ×` the size the viewport
resolves it to: `22px`/`≤25.3px` at 390 px, `25.8px`/`≤29.7px` at 820 px and the `28px` ceiling
with `line-height` `≤32.2px` at 1280 px. The rendered question occupies at most 6 line boxes at
390 px and at most 4 above it, for a question at the 160-character bound as well as for the
133-character card the tests measure.

*Superseded 2026-09-18.* The wording this replaces read: "`font-size: 28px` with `line-height`
≤ `32.2px` at a 390 px viewport, and `font-size: 36px` with `line-height` ≤ `39.6px` at
1280 px." 28px put a 160-character question on six lines of a 390×844 phone before the reader
reached the context, and nothing bounded the line count at any width. George, 2026-09-18: "it's
up but it's squished and not mobile responsive"; "the web version looks mushed up ... (iPad has
it squished up)".

*And the desk figure with it, the same day.* `36px` came from the deck's own
`@media(min-width:900px){.item>h2{font-size:2.25rem;line-height:1.1}}`, which spent five lines
of an 800 px-tall desk window on a 160-character question while the answers waited below it.
That override is gone: one declaration sizes the question at every width, its ceiling raised
from `1.625rem` to `1.75rem` so the desk keeps a headline rather than a large paragraph, and
`28px` at 1280 px is the same figure this requirement asked of the PHONE at the baseline.

**WEB-03** — no glyph prefix anywhere.
Strength: MUST · Layer: unit · Source: plan §critique ("a `> ` glyph in front"), §Principles 5.
`CSS` contains no `content:'>'` and no `content:'> '` declaration (the baseline has two:
`rust/serve.rs:4812`, `rust/serve.rs:4829`), and no rendered heading or link text produced by
`serve.rs` begins with `>` or ends with `→`.

**WEB-04** — the type scale is the plan's, and nothing is tracked out.
Strength: MUST · Layer: unit · Source: plan §Type scale.
`CSS` declares `h1` at `1.5rem`/`1.2`, section `h2` at `1.0625rem`/`1.3` weight `600` in
`var(--subtext)`, body at `1rem`/`1.55`, meta at `.8125rem`/`1.4`, the button label at
`1rem`/`1.2` weight `600`, and the digit badge at `.75rem` in `var(--mono)`. `CSS` contains no
`text-transform` declaration and no `letter-spacing` declaration.

**WEB-05** — mono is for things that are literally code.
Strength: MUST · Layer: unit · Source: plan §Type.
Every selector in `CSS` that names `var(--mono)` is one of: `code`, `.id`, `.key` (the digit
badge), `.priority`, `p.keys`, `td.n`, `th.n`. No other selector names it.

**WEB-06** — prose stays readable in line length.
Strength: SHOULD · Layer: unit · Source: plan §Type ("Line length ≤ 70ch for prose").
`.context`, `.consequence`, `.explain`, `.item>h2` and `.body` each declare `max-width:70ch`.

**WEB-07** — the question is the largest thing on the screen.
Strength: MUST · Layer: chrome · Source: plan §Principles 1.
On `/` with at least one open item, the current card's `h2` has a strictly greater computed
`font-size` than every other element rendered in the viewport.

### Surfaces and colour

**WEB-08** — the token block is exactly the plan's.
Strength: MUST · Layer: unit · Source: plan §Color.
`CSS`'s `:root` declares `--base:#1e1e2e`, `--mantle:#181825`, `--surface0:#313244`,
`--surface1:#45475a`, `--text:#cdd6f4`, `--subtext:#a6adc8`, `--overlay:#9399b2`,
`--green:#a6e3a1`, `--red:#f38ba8`, `--yellow:#f9e2af`, `--blue:#89b4fa`, `--link:#89b4fa`,
`--focus:#b4befe`, and no other colour token. No hex literal appears anywhere in `CSS` outside
this block. (`--overlay` differs from the plan's `#6c7086`: see §7 OQ-1.)

**WEB-09** — no outlines except the focus ring; one hairline.
Strength: MUST · Layer: unit · Source: plan §Color ("No outlines except the focus ring"),
§Principles 2.
Every `border`, `border-*` and `outline` declaration in `CSS` belongs to the allowlist: the
`:focus-visible` ring; the single hairline `1px solid var(--surface1)` between the card body and
the answer panel; the single row hairline `1px solid var(--surface0)` on `tr`/row separators; the
`3px` left rule on a history receipt; the `2px` left rule on the drawer's current page. Any other
border or outline declaration fails this requirement.

**WEB-10** — two radii, each with one job.
Strength: MUST · Layer: unit · Source: plan §Review pass 2, trait 4.
Every `border-radius` value in `CSS` is `8px` (buttons, inputs, the note field) or `999px`
(pills). The baseline's `4px`, `6px` and `10px` radii (`rust/serve.rs:4815`,
`rust/serve.rs:4832`, `rust/serve.rs:4846`) are gone.

**WEB-11** — a colour is an outcome.
Strength: MUST · Layer: unit · Source: plan §Principles 3.
`var(--green)`, `var(--red)`, `var(--yellow)` and `var(--blue)` appear in `CSS` only on
selectors that carry an outcome or a status: `.choice.outcome-*`, `.receipt.outcome-*`,
`.pill.status-*`, `.priority-p0`, and `--link`/`--focus` through their own tokens. Exactly one
selector gives an answer button a coloured `background`, and it is the recommended choice.

**WEB-12** — the recommendation leads, on fill.
Strength: MUST · Layer: chrome · Source: plan §Layout, §critique ("the recommendation does not
lead").
On `/`, the recommended answer's computed `background-color` is its outcome hue and its
computed `color` is `--base`; every alternative's computed `background-color` is `--surface0`;
`document.body`'s is `--base`; the answer panel, the side column and the open drawer compute
`--mantle`.

**WEB-13** — nothing is boxed.
Strength: MUST · Layer: chrome · Source: plan §Principles 2.
On `/`, `/all`, `/decided`, `/boards` and `/board/<board>`, no rendered element computes a
non-zero border width other than the allowlist of WEB-09, and no element computes an `outline`
width > 0 unless it holds keyboard focus.

### Motion and feedback

**WEB-14** — one motion exists in the stylesheet.
Strength: MUST · Layer: unit · Source: plan §Motion, §Principles 4.
`CSS` declares exactly three `@keyframes` — the advance out, the advance in, and the back-step
out — and no `transition` declaration other than the phone drawer's `transform`. It declares no
`animation` on a notice, a receipt, the live indicator or a hover state. The baseline's `pulse`,
`notice-in` and receipt-landing animations (`rust/serve.rs:4854`, `rust/serve.rs:4865`) are gone.

**WEB-15** — the advance is 140 ms each way.
Strength: MUST · Layer: chrome · Source: plan §Motion.
The leaving card computes `animation-duration: 0.14s` with an `-16px` X translation at its
end frame; the entering card computes `animation-duration: 0.14s` from `+16px`.

**WEB-16** — a projection refresh never re-animates the current card.
Strength: MUST · Layer: chrome · Source: plan §Motion; George: "it keeps blinking".
Given the deck's current card is unchanged, when a `/live` notice triggers a projection refresh,
then the current card's DOM node is the same object (a property set on it before the refresh is
still readable afterwards), it carries no entering class, and its `getAnimations()` list is
empty.

**WEB-17** — the pressed answer says what it is doing on its own fill.
Strength: MUST · Layer: chrome · Source: plan §Motion (feedback).
After a click, the pressed button's text is `Sending…`, its computed `background-color` is
unchanged from before the click, it is not `[disabled]`, and every other answer computes an
`opacity` < 1.

**WEB-18** — the receipt encodes its outcome.
Strength: MUST · Layer: chrome · Source: plan §Motion (feedback), §Review pass 2.
The landed history row computes `border-left-width: 3px` in the hue of the outcome just
recorded, and no other row on the page carries a coloured left rule.

**WEB-19** — the toast is readable and dismissible.
Strength: MUST · Layer: chrome · Source: plan §Motion; George: "the toast is too fast".
A toast stays at least 20000 ms; hovering it or focusing it stops that timer for as long as the
pointer or focus is on it; a click on it and the `Esc` key each remove it immediately; at most
three are on screen at once and the newest is first in DOM order.

**WEB-20** — reduced motion removes the advance too.
Strength: MUST · Layer: chrome · Source: plan §Motion.
With `prefers-reduced-motion: reduce` emulated, answering a card changes the current card with
`animation-name: none` on both the leaving and the entering node, and the queue still advances.

**WEB-21** — the long form says there is more.
Strength: SHOULD · Layer: unit · Source: plan §Layout ("bottom fade 2rem to signal more").
The deck's scrolling body region declares a `2rem` bottom fade (a `mask-image` or
`-webkit-mask-image` linear gradient), and no other element declares one.

### Copy

**WEB-22** — meta is a sentence, never a chain.
Strength: MUST · Layer: unit · Source: plan §Copy, §Principles 5.
No string RENDERED BY THE SERVER inside `.eyebrow`, `.meta`, a list row's meta line or a receipt
contains ` · `, ` | ` or a trailing `→`, and the inline script writes exactly one ` · ` and no
`→`. Two separators are allowed, each named: the single keyboard hint line of WEB-27, because it
is a key map and not meta; and the deck's own eyebrow join.

*Superseded 2026-09-18 for the second of those.* The wording this replaces read: "No string
rendered inside `.eyebrow` ... contains ` · ` ... (The single keyboard hint line of WEB-27 is
the one place a separator is still allowed ...)". On the deck the script now moves the card's
meta line onto the end of its eyebrow, joined with ` · `, so the priority pill stops sitting
directly under the last line of the long form as it dissolves into the fade — the pill over
clipped text George objected to on 2026-09-17, which trailing had only moved a few pixels down
(George, 2026-09-18, on the v6 screenshots). One line of orientation carries who asked, where,
when and how urgent; the SERVED markup still renders two paragraphs in the ADR-042 §5 order, so
a scriptless page is unchanged, and the script's one separator is counted rather than banned.

**WEB-23** — the eyebrow names who asked, where, and when, in one sentence.
Strength: MUST · Layer: unit · Source: plan §Layout, §Copy.
A card's eyebrow renders exactly `<raiser> asked on <board>, <age>` — e.g. `codex@driver asked
on px, 3 days ago` — with the board as the only link in it, and the kind, the priority and the
word `waiting` absent from it (the baseline renders all three: `rust/serve.rs:1836`).

**WEB-24** — the note field's label, and only its label.
Strength: MUST · Layer: unit · Source: plan §Copy.
The note field's label text is exactly `Add a note`, and the deck renders no hint paragraph
beneath it (the baseline renders `Add a note (optional)` plus a two-sentence hint:
`rust/serve.rs:1877`-`rust/serve.rs:1881`).

**WEB-25** — the custom answer's button says what happens.
Strength: MUST · Layer: unit · Source: plan §Copy.
The custom submit's label is exactly `Record my answer` (the baseline says `Record this
answer`: `rust/serve.rs:1887`).

**WEB-26** — the empty deck is an answer, not an absence.
Strength: MUST · Layer: unit · Source: plan §Copy.
The empty-deck copy is exactly `Nothing is waiting. Every question an agent raised has an
answer.` followed by a link whose text is exactly `See what was decided` and whose target is
`/decided`.

**WEB-27** — one quiet keyboard line, and the digits live on the buttons.
Strength: MUST · Layer: unit · Source: plan §Layout, §Principles 6.
Each deck page renders exactly one `p.keys` element, its text is `1–4 answer · s skip · u undo ·
c own`, it renders no `<kbd>` element, `CSS` styles it at `.75rem` in `var(--overlay)` and
`var(--mono)`, and each answer button contains its own digit in a `.key` span.

**WEB-28** — counts read as counts.
Strength: MUST · Layer: unit · Source: plan §Copy.
The deck bar renders `<n> left`, where `<n>` is the number of open items still in the deck's
queue across every registered board; `/all` renders `<n> open across <m> boards`, where the two
numbers are the open-item count and the board count of that same aggregate
(`rust/serve.rs:1500`).

**WEB-29** — a board refusal is the board's own sentence.
Strength: MUST · Layer: unit · Source: plan §Copy.
A decision the board refused renders the store's refusal text inside
`p.error[data-refusal=board]`, verbatim, and `CSS` styles `p.error` with `color:var(--red)`, no
`background` and no `border`. The two refusal channels are distinguished by that attribute: the
board's own sentence is `data-refusal=board`, the composer's pre-flight sentence is
`data-refusal=incomplete` (WEB-57); no other value exists and no element carries both.

### Deck behaviour

**WEB-30** — answering advances to the next card.
Strength: MUST · Layer: chrome · Source: plan §Motion; ADR-042 §5.
Clicking the recommended answer, or pressing `1`, records the decision and leaves the next card
current, with the deck position reading `2 of n`.

**WEB-31** — skip moves the card to the back and records nothing.
Strength: MUST · Layer: chrome · Source: plan §Layout (`s skip`).
Pressing `s` makes the next card current, leaves the skipped card last in the queue, and the
board's attention row stays `open` with no `decision`.

**WEB-32** — undo brings the last decision back.
Strength: MUST · Layer: chrome · Source: ADR-016 amendment; plan §Layout (`u undo`).
Pressing `u` after a decision reopens that item on the board and returns it to the deck as the
current card.

**WEB-33** — a refused decision returns the card with the reply intact.
Strength: MUST · Layer: chrome · Source: plan §Copy (refusal), §Motion.
When the board refuses the write, the card that was sent becomes current again in its own slot,
its refusal sentence renders per WEB-29, and any text typed into the note is still in the field.

**WEB-34** — the last card leaves the empty state.
Strength: MUST · Layer: chrome · Source: plan §Copy.
Answering the only remaining card renders the WEB-26 copy and its link, and no card element
remains in the deck.

**WEB-35** — the page never scrolls on the deck; the card's own column does, and the long form
inside it.
Strength: MUST · Layer: chrome · Source: plan §Layout; George, 2026-09-18.
At 390, 820 and 1280 px wide, `documentElement.scrollHeight <= documentElement.clientHeight` on
`/` with a card whose body is longer than the viewport; the current card's own element is the one
scroller that carries the overflow of the whole card, the long-form body region's
`scrollHeight > clientHeight` inside its 40vh cap, and the answer panel has no scroll of its own
(`scrollHeight == clientHeight`). The raiser's context is the body region's FIRST block on the
deck, so the two share that one capped, fading scroller; the long-form region's bottom edge is
never below the answer panel's top edge; the recommended answer's bounding box is fully inside
the viewport on the screen the card opens on, at all three widths, for a card at the product's
bounds (160-char question, 793 of context, four answers); the keyboard line is inside the
viewport whatever the card is scrolled to; no two answer buttons share a row and each is as wide
as the answers' own content width within 1 px; and scrolling the card's column to its end puts
the note field fully inside the viewport and above the keyboard line. The priority pill's rect
bottom is above the question's rect top, and nothing carrying text stands in the band between
the long-form region's bottom edge and the answer panel's top edge. A projection swap that
replaces the current card's node restores both scroll positions — the card's column and the body
region inside it — whenever the same item is still the card on screen.

*Superseded 2026-09-18.* The wording this replaces read: "the page never scrolls on the deck;
the body region does. ... `documentElement.scrollHeight <= documentElement.clientHeight` ... and
that body region's `scrollHeight > clientHeight`." It was satisfied by a panel capped at three
fifths of the card that scrolled its own answers — a second, unannounced scroller which put the
note field at y=961 on an 844 px screen and cut the fourth answer at the cap (George,
2026-09-18, on the served deck). One scroller now carries the card, and the note is reached by
the gesture the reader is already making.

*Superseded again, the same day, by what that first fix cost.* Two things followed from making
the card the one scroller, and both are now part of this requirement rather than left as
consequences:

- With the context a free block in the card's column, 793 characters of it pushed the
  recommended answer to y=948 on a 390×844 phone — off the screen the card opens on, which is
  the one thing a deck is for. The context is INSIDE the capped, fading body region now, as its
  first block, so the paragraph and the long form it introduces scroll together under one soft
  edge. The served markup is unchanged: ADR-042 §5's order (question, context, answers, folded
  body) is what a scriptless browser still reads, and the deck's own script moves the paragraph.
  George's 2026-09-17 objection was to a hard cut with the priority pill over it, not to a
  scrollable region with a fade.
- A `/live` notice that replaces the current card's node used to hand it back scrolled to the
  top, because the position now lives on a node the swap replaces. Both positions are carried
  across the swap.
- The card's trailing meta line, the priority pill in it, ended up directly under the last line
  of the long form as that line dissolves into the fade, with the two-rem band empty below it:
  the pill over clipped text of 2026-09-17, moved a few pixels down (George, 2026-09-18, on the
  v6 screenshots). On the deck the script moves the meta's contents onto the end of the eyebrow,
  above the question, where it is orientation and cannot be read as an answer. WEB-22 records
  the separator that join uses. The served order is again unchanged.

**WEB-57** — the composer's own refusal is its own sentence, in its own channel.
Strength: MUST · Layer: chrome · Source: `rust/serve.rs:3850` (`INCOMPLETE_ANSWER`); ADR-042 §1
(the custom answer carries both halves).
When the own-words answer is submitted with only one half — a reply with no verdict, or a verdict
with no reply — the page refuses before posting, renders exactly
`Your own answer needs both halves: pick a verdict (approve, reject, defer or other) and write
your reply.` inside `p.error[data-refusal=incomplete]`, moves focus to the missing half (the
verdict radio group when the verdict is missing, the reply field when the reply is missing), and
the board records nothing: the item's `status` stays `open` and its `decision` stays absent. The
sentence is one fixed string and never names which half is missing.

### Navigation

**WEB-36** — the drawer is rows on the desk surface.
Strength: MUST · Layer: chrome · Source: plan §Layout (Navigation).
Opening the menu shows the drawer with computed `background-color` `--mantle`, each destination
as a row separated by the one `1px` `--surface0` hairline, and the search field above them.

**WEB-37** — the current page is marked by a rule, not a fill.
Strength: MUST · Layer: chrome · Source: plan §Layout (Navigation).
On each destination, that destination's drawer link computes `border-left-width: 2px` in `--text`
and a `background-color` equal to the drawer's own; no other drawer link computes a left border.

**WEB-38** — navigation works with no script at all.
Strength: MUST · Layer: http · Source: ADR-016; plan §Layout.
The served HTML of a page contains an anchor for every destination (`data-nav` values
`needs-you`, `all`, `decided`, `lanes`, `boards`, `sprints`, `plans`, `deployments`,
`subscriptions`), and a `GET` of each `href` over HTTP with no browser returns `200` with that
page's heading. Since `t-1f495a7f` the drawer is read from `/all`: `/` is the mounted deck's
shell and carries no navigation of its own, and the one thing `/` must answer without a script
is that shell's mount point (spec SPA-51).

**WEB-39** — search is the first thing in the drawer.
Strength: MUST · Layer: unit · Source: plan §Layout (Navigation).
In the served drawer markup, the search `input` precedes every destination anchor in document
order.

### Without a script

**WEB-56** — with scripting off, `/` is a plain list that still works.
Strength: MUST · Layer: chrome · Source: ADR-016; `rust/serve.rs:3747`-`rust/serve.rs:3754`
(every deck rule is scoped to `html.js`).
With JavaScript disabled in the browser, `/` renders every open card visible in the server's
order — no card computes `display: none` or `visibility: hidden`, and none carries a
`data-current`-only presentation — the document scrolls
(`documentElement.scrollHeight > documentElement.clientHeight` when the cards exceed the
viewport), every card's `form.decide` submits its `decision` value to
`/attention/<board>/<id>/reply` and lands the decision on the board, and the primary nav and its
drawer markup are present and reachable without the menu script.

**WEB-58** — the deck is a progressive enhancement in the stylesheet, not a requirement.
Strength: MUST · Layer: unit · Source: `rust/serve.rs:3754`; plan §Layout.
Every rule in `CSS` that hides a card, positions the answer panel, sizes the deck column or
animates the advance is scoped to a `html.js` ancestor selector, so a page whose script did not
run keeps the plain list. A deck rule at document scope fails this requirement.

### Read pages

**WEB-40** — the row is the unit, and its meta is a sentence.
Strength: MUST · Layer: unit · Source: plan §Layout (every other page).
A list row renders the title as the first element — as an anchor exactly when the row opens a
page — followed by one meta sentence per WEB-22; the priority renders as plain `var(--mono)`
text in `--overlay`, `P0` in `--red`, and no priority is rendered as a pill.

**WEB-41** — one pill style everywhere.
Strength: MUST · Layer: unit · Source: plan §Layout (status pill).
`CSS` declares exactly one pill rule — `.pill` with `background:var(--surface0)`,
`font-size:.75rem`, `border-radius:999px` — plus status hue modifiers `.pill.status-*` that set
only `color`. The baseline's `.tag`, `.kind` and `.priority` pill rules are gone, and no served
markup uses a second badge class.

**WEB-42** — tables lose their borders and keep the hairline.
Strength: MUST · Layer: unit · Source: plan §Layout (tables).
In `CSS`, `table`, `th` and `td` declare no border other than the row-separating
`1px solid var(--surface0)`; `th` is `.75rem` in `var(--overlay)`; `td.n`/`th.n` declare
`text-align:right` and `var(--mono)`.

**WEB-43** — and it holds in the browser.
Strength: MUST · Layer: chrome · Source: plan §Layout (tables).
On `/boards` and `/deployments`, every `th` and `td` computes a total border width of `1px` or
`0px`, that `1px` is the row separator in `--surface0`, and every numeric cell computes
`text-align: right` with the `--mono` family.

### Responsiveness

**WEB-44** — nothing overflows sideways, anywhere.
Strength: MUST · Layer: chrome · Source: plan §Layout; George's devices.
At 390, 820 and 1280 px wide, every in-scope route satisfies
`documentElement.scrollWidth <= documentElement.clientWidth`.

**WEB-45** — a thumb can hit every deck control.
Strength: MUST · Layer: chrome · Source: Apple Human Interface Guidelines' 44 pt minimum hit
target; George, 2026-09-17: "use SDD to do the UI/UX using best practices"; plan §Layout (phone
deck).
At 390 px wide on `/`, every answer button, the custom submit, the menu button and the history
button has a bounding box at least 44 × 44 CSS px. **This is not the WCAG figure.** WCAG 2.2 AA
SC 2.5.8 asks for 24 × 24 CSS px; 44 is a deliberate floor above AA, taken from the platform
George actually decides on (iPhone Safari) and recorded in §6 as its own commitment rather than
as conformance.

**WEB-46** — the desk becomes two columns on a Mac, and a drawer between.
Strength: MUST · Layer: chrome · Source: plan §Layout (deck ≥ 900px).
At 1280 px the deck column's width is ≤ 44rem and the side column's is 22rem on `--mantle`
holding the toasts above the session history; at 820 px the side column is a drawer behind the
history button and is hidden until that button is pressed.

**WEB-47** — every answer gets a row of its own.
Strength: MUST · Layer: unit · Source: George, 2026-09-18 (the served deck at 390 and 820).
`.alternatives` declares `display:grid` with `grid-template-columns:1fr`, so every answer is one
row at every width, and the answer panel's rule declares neither `max-height` nor an `overflow`
of its own, so the card's own column is the deck's one scroller. What that renders as is
measured in the browser by WEB-35.

*Superseded 2026-09-18.* The wording this replaces read: "three or four alternatives may wrap.
Strength: MAY ... The alternatives container may lay out two per row and wrap; nothing in this
slice requires a single row." Two-up rendered a 37-character label as three or four wrapped
lines inside a 177 px button on a 390 px phone and as two lines in a 384 px button at 820 px
(George, 2026-09-18: "it's up but it's squished and not mobile responsive"). The strength rises
from MAY to MUST because the single row is now the requirement rather than the tolerance.

### Accessibility

The target is WCAG 2.2 level AA for the surfaces this slice restyles.

**WEB-48** — text contrast is at least 4.5:1 on the surface it sits on.
Strength: MUST · Layer: unit · Source: WCAG 2.2 SC 1.4.3; plan §Color.
Computed from the WEB-08 tokens, every foreground/background pair the stylesheet actually uses
is ≥ 4.5:1. The measured ratios are: `--text` on `--base` 11.34, on `--mantle` 12.14, on
`--surface0` 8.69; `--subtext` on `--base` 7.37, on `--mantle` 7.89, on `--surface0` 5.65;
`--overlay` on `--base` 5.81, on `--mantle` 6.22; `--base` on `--green` 11.03, on `--red` 7.08,
on `--yellow` 12.91, on `--blue` 7.79; `--green` on `--surface0` 8.46, `--red` 5.43,
`--yellow` 9.89, `--blue` 5.97; `--link` on `--base` 7.79, on `--mantle` 8.34. The unit test
computes these ratios from the tokens rather than pinning the numbers, so a token change that
drops a pair below 4.5:1 fails.

**WEB-49** — `--overlay` never sits on `--surface0`.
Strength: MUST · Layer: unit · Source: WCAG 2.2 SC 1.4.3; the arithmetic in WEB-48.
`--overlay` on `--surface0` is 4.45:1, below AA for text under 24px. No `CSS` rule sets
`color:var(--overlay)` on a selector whose background is `var(--surface0)`; quiet text inside a
pill uses `--subtext` (5.65:1).

**WEB-50** — non-text contrast where it identifies something.
Strength: MUST · Layer: unit · Source: WCAG 2.2 SC 1.4.11.
The focus ring is ≥ 3:1 against both desk surfaces (`--focus` on `--base` 9.17, on `--mantle`
9.81), and the recommended answer's fill is ≥ 3:1 against the page (`--green` on `--base`
11.03, `--red` 7.08, `--yellow` 12.91, `--blue` 7.79). The one hairline and the alternative
answers' `--surface0` fill are separators and grounds, not the boundary that identifies a
control — the control is identified by its label text, which passes WEB-48 — so 3:1 is not
asserted on them; §7 OQ-2 carries this to George.

**WEB-51** — focus is always visible.
Strength: MUST · Layer: chrome · Source: WCAG 2.2 SC 2.4.7, 2.4.11; plan §Color (focus ring).
Tabbing through `/` and `/boards`, every focusable element computes an `outline-width` ≥ 2px in
`--focus` while focused, and no rule sets `outline: none` without a replacement indicator.

**WEB-52** — two announcement channels, each with one role.
Strength: MUST · Layer: unit · Source: WCAG 2.2 SC 1.3.1, 4.1.3; clarifies the baseline, which
gives `role=status` to both the connection line (`rust/serve.rs:1594`) and the toast strip
(`rust/serve.rs:1624`).
On every in-scope route the served markup satisfies all four:

1. Each `input` and `textarea` is named by a `label` whose `for` matches its `id`, or by
   `aria-label`.
2. The connection line is **exactly one** element per page carrying `role=status` with
   `aria-live=polite`.
3. The toast strip is a **separate** region carrying `role=log` with `aria-live=polite`, and it
   **MUST NOT** carry `role=status`. No third live region exists.
4. Each card's `aria-labelledby` resolves to its question.

This is a clarification, not a collapse: the baseline already renders both regions, and both keep
announcing. What changes is that they stop claiming the same role, so a notice is a log entry and
the connection state is a status.

**WEB-59** — and each channel says only its own thing.
Strength: MUST · Layer: chrome · Source: WEB-52; `rust/serve.rs:3755`, `rust/serve.rs:3765`.
In Chrome, the connection line's text is only ever one of `connecting`, `live`, `reconnecting` or
`sending`; a `/live` notice and a decision receipt appear as children of the `role=log` region
and never change the connection line's text; and the `role=log` region never gains
`role=status`.

**WEB-53** — the card is named by its question.
Strength: MUST · Layer: chrome · Source: WCAG 2.2 SC 4.1.2; plan §Principles 1.
The current card's accessible name, read from the accessibility tree, equals its question text.

### Security and quality

**WEB-54** — the document reaches no third party.
Strength: MUST · Layer: unit · Source: ADR-016:49; plan §Type ("no downloads (CSP self)").
Every `href` and `src` in the served markup of every in-scope route is same-document (`#…`) or
site-absolute (`/…`); the document contains no `<link>`, `<img>`, `<iframe>` or `<script src>`.
The stylesheet and script stay inline in the document, so the edge's existing `self`-scoped
policy keeps applying unchanged; kanban itself sends no `Content-Security-Policy` header at the
baseline and this slice adds none.

**WEB-55** — the HTTP surface does not move.
Strength: MUST · Layer: unit · Source: ADR-016:82; non-goals.
`render`'s route arms and `post`'s allowlist match the baseline set exactly: the sixteen read
routes plus the not-found arm (`rust/serve.rs:402`-`rust/serve.rs:442`), and the four POST verbs
`attention/../reply`, `attention/../reopen`, `plan/../open`, `subscription/../pause|resume`
(`rust/serve.rs:453`-`rust/serve.rs:459`). A route or verb added or removed fails this test.

**Observation, not a requirement.** At the baseline the inline stylesheet is 26,213 bytes and
the inline script is 52,211 bytes (`rust/serve.rs:4799`-`rust/serve.rs:5160`,
`rust/serve.rs:3747`-`rust/serve.rs:4782`). This is recorded so a later reader can see the delta.
It is **not** a budget, and no requirement in this slice constrains it. Only George may turn it
into one.

## 4. Acceptance

Plain-language Given/When/Then. One scenario per requirement group where a single scenario
proves the group; the IDs it proves are named on each scenario.

**A1 — the hero (WEB-01, WEB-02, WEB-04, WEB-07).**
*Given* a board with one open carded attention item and `kanban serve` running,
*when* George opens `/` in Chrome at 390 px and then at 1280 px,
*then* the question is set in the `--serif` stack at weight 600, 22px/≤25.3px on the phone and
28px/≤32.2px on the Mac — one `clamp` declaration, no breakpoint of its own — no other element
on screen is larger, and the question occupies at most 6 line boxes on the phone and at most 4
on the Mac whether it is 133 or 160 characters long.

**A2 — no glyphs, no tracking, no chains (WEB-03, WEB-22, WEB-23, WEB-27, WEB-28, WEB-40).**
*Given* its own fixture — board `px` with 3 open carded items, the oldest raised by
`codex@driver` 3 days ago, and board `atmux` with 9 open items, 12 open in total across the 2
registered boards —
*when* the served HTML of `/`, `/all`, `/decided` and `/boards` is read over HTTP,
*then* `/all` reads exactly `12 open across 2 boards`, the deck bar on `/` reads exactly
`12 left` and reads exactly `3 left` once the nine `atmux` items are decided, the oldest card's
eyebrow reads exactly `codex@driver asked on px, 3 days ago`, no `.eyebrow`, `.meta` or list-row
meta string contains ` · `, `/` contains exactly one `p.keys` element and zero `<kbd>` elements,
and no `h1` or `h2` text begins with `>`.

**A3 — surfaces, not boxes (WEB-08, WEB-09, WEB-10, WEB-11, WEB-12, WEB-13).**
*Given* the deck with a recommended choice and two alternatives, *when* the stylesheet is read
and the page is measured in Chrome, *then* the token block is the only place a hex appears, every
border and outline belongs to the WEB-09 allowlist, the only radii are 8px and 999px, the
recommended answer is filled in its outcome hue with `--base` text, the alternatives are
`--surface0`, and the page, panel and side column are `--base`/`--mantle`/`--mantle`.

**A4 — one motion (WEB-14, WEB-15, WEB-16, WEB-20).**
*Given* the deck showing card 1 of 3, *when* George answers card 1, *then* card 1 leaves left
over 140 ms and card 2 enters from the right over 140 ms; *and when* a `/live` notice arrives
while card 2 is current and unchanged, *then* card 2's node is not replaced and does not
re-animate; *and given* `prefers-reduced-motion: reduce`, *when* George answers, *then* the
queue advances with no animation at all.

**A5 — feedback (WEB-17, WEB-18, WEB-19).**
*Given* a card with three answers, *when* George clicks one and the board is slow to answer,
*then* that button reads `Sending…` on its own fill and the other two dim; *when* the board
answers, *then* a receipt lands with a 3px left rule in the recorded outcome's hue and a toast
appears; *when* George hovers the toast for 25 s, *then* it is still there; *when* he clicks it
or presses `Esc`, *then* it goes; *when* four decisions land in quick succession, *then* three
toasts are on screen, newest first.

**A6 — the deck as a queue (WEB-30, WEB-31, WEB-32, WEB-34, WEB-35).**
*Given* three open items, *when* George presses `1`, *then* the decision is recorded and the
position reads `2 of 3`; *when* he presses `s`, *then* the next card is current and the skipped
item is last in the queue with its board row still `open` and no `decision`; *when* he presses
`u`, *then* the item he just decided is reopened on the board and is current again; *when* he
answers the last card, *then* `Nothing is waiting. Every question an agent raised has an answer.`
and `See what was decided` are on screen; throughout, the page itself never scrolls, the card's
own column is the one thing that carries the card's overflow, and the long form scrolls inside
its own region.

**A7 — refusal (WEB-29, WEB-33).**
*Given* a card whose board will refuse the write (the item was settled by another process),
*when* George types `hold it` into the note and clicks an answer, *then* the card is the current
card again at the same queue position, the board's refusal text is in
`p.error[data-refusal=board]` whose computed `color` is `rgb(243, 139, 168)` with
`border-width: 0px` and the panel's own `background-color`, and the note field's `value` is still
`hold it`.

**A8 — the incomplete own answer (WEB-57).**
*Given* the own-words fold open with `ship it monday` typed and no verdict picked,
*when* George presses `Record my answer`,
*then* `p.error[data-refusal=incomplete]` contains exactly `Your own answer needs both halves:
pick a verdict (approve, reject, defer or other) and write your reply.`, `document.activeElement`
is the first verdict radio, and `attention show --json` for that item still reports
`"status": "open"` with no `decision` object. *And given* a verdict picked with the reply empty,
*when* he submits, *then* the same sentence appears and `document.activeElement` is the reply
textarea.

**A9 — navigation (WEB-36, WEB-37, WEB-39).**
*Given* `/boards`, *when* George opens the menu, *then* the drawer's computed
`background-color` is `rgb(24, 24, 37)`, the search `input` precedes all nine destination anchors
in document order, each anchor computes `border-bottom: 1px solid rgb(49, 50, 68)`, and the
`Boards` anchor alone computes `border-left-width: 2px` with `border-left-color: rgb(205, 214,
244)` while its `background-color` equals the drawer's — every other anchor computes
`border-left-width: 0px`.

**A10 — no script at all (WEB-38, WEB-56, WEB-58).**
*Given* JavaScript is never executed — a plain HTTP client, and then Chrome with scripting
disabled — and a board with 3 open items,
*when* every `data-nav` href from `/` is fetched and `/` itself is loaded,
*then* each destination returns `200` containing that page's `h1` text; all 3 cards are in the
DOM in the server's order with computed `display` not `none` and `visibility` not `hidden`; the
document scrolls (`documentElement.scrollHeight > documentElement.clientHeight`); the primary
nav is visible; and *when* the first card's form is submitted with its recommended
`decision` value, *then* the response is a `303`/`200` reply page and `attention show --json`
reports that item `"status": "resolved"` with the matching `decision.choice`.

**A11 — read pages (WEB-40, WEB-41, WEB-42, WEB-43).**
*Given* a board with one task in each of `todo`, `in_progress`, `blocked`, `review` and `done`,
one of them `P0`, *when* `/board/<board>`, `/boards` and `/deployments` are read and measured,
*then* each row's first child element is its title (an `a` exactly when the row opens a page)
followed by exactly one meta string containing no ` · `; every status renders as `span.pill` and
no other badge class appears in the markup; the `P0` row's priority is a `span.priority` whose
computed `font-family` is the `--mono` stack and whose computed `color` is `rgb(243, 139, 168)`;
and every `th`/`td` computes a total border width of `1px` (the row separator, `rgb(49, 50, 68)`)
or `0px`.

**A12 — devices (WEB-44, WEB-45, WEB-46).**
*Given* every in-scope route, *when* each is loaded at 390, 820 and 1280 px, *then*
`documentElement.scrollWidth <= documentElement.clientWidth` holds on every one; at 390 every
answer button, the custom submit, the menu button and the history button has
`getBoundingClientRect()` width and height ≥ 44; at 1280 the deck column's width is ≤ 704px
(44rem) and the side column's is 352px (22rem) with computed `background-color`
`rgb(24, 24, 37)`; at 820 the side column's `offsetParent` is `null` until the history button is
clicked, and non-`null` after.

**A13 — accessibility (WEB-48, WEB-49, WEB-50, WEB-51, WEB-52, WEB-53, WEB-59).**
*Given* the token block, *when* the ratios are computed, *then* every used text pair is ≥ 4.5:1,
`--overlay` is never placed on `--surface0`, and the focus ring and the filled answer are ≥ 3:1
against their grounds; *when* George tabs through `/` and `/boards`, *then* every focusable
element shows a computed `outline-width` ≥ 2px in `rgb(180, 190, 254)`; *and* the current card's
accessible name equals its question text, every `input`/`textarea` has a programmatic label,
exactly one element per page carries `role=status` (its text only ever `connecting`, `live`,
`reconnecting` or `sending`), and the toast strip carries `role=log` and never `role=status`.

**A14 — the surface does not move (WEB-54, WEB-55).**
*Given* the restyled build, *when* the served markup and the route table are read, *then* no
`href`/`src` leaves the site, there is no `<link>`/`<img>`/`<iframe>`/`<script src>`, and the
sixteen read routes and four POST verbs are exactly those of the baseline.

**A15 — the deck on the three screens George decides on (WEB-02, WEB-22, WEB-35, WEB-47).**
*Given* board `DECKSTACK` with two open carded items, both with a body far longer than any of
the viewports: the card at the product's bounds — a 160-char question, 793 of context, four
authored choices each with its consequence, one recommended — at `P0`, and the shipped fixture
card — 133-char question, 452 of context, three choices — at `P1`,
*when* `/` is loaded in real Chrome at 390 × 844,
*then* the deck opens the `P0` card at its own top (`scrollTop` 0) with the raiser's context
inside the long-form region rather than above it and the card's meta joined onto the end of the
eyebrow above the question; `documentElement.scrollWidth` is 390; the four answers are at `top`
502, 649, 789 and 908, no two sharing a row, each 367 px wide, which is the answers' own content
width; the RECOMMENDED answer's box is 502–563, inside the 844 px viewport — the screen the card
opens on; `form.decide` reports `scrollHeight == clientHeight == 755`; the long-form region
reports `scrollHeight` 6226 over `clientHeight` 118 with its bottom edge (443) above the panel's
top edge (450) and nothing carrying text standing between them; `p.keys` is at 816–844; the
priority pill is at 156–171, above the question's top (177); and the question is 5 line boxes
of 22px.
*And when* the card's own column is scrolled to its end (396 px, its full extent),
*then* the note field is at 680–749 — fully inside the viewport and above `p.keys`, which is
still at 816–844 — and the panel's own foot (809) is above it.

*And when* the same page is loaded at 820 × 1180,
*then* the four answers are at `top` 574, 683, 785 and 887, each 782 px wide, the recommended
one 574–618; the card's column needs no scroll at that height (`scrollHeight == clientHeight ==
991`), so the note is already at 1016–1085 with `p.keys` at 1152–1180; the panel reports
`scrollHeight == clientHeight == 623`; the long form reports 4292 over 228 with its bottom (515)
above the panel's top (522); the pill is at 156–171 above the question's 177; and the question
is 3 line boxes of 25.8px.

*And when* the same page is loaded at 1280 × 800,
*then* the deck column is 704 px with the side column beside it; the four answers are at `top`
482, 591, 692 and 794, each 666 px wide, the recommended one 482–526 inside the 800 px viewport;
the panel reports `scrollHeight == clientHeight == 623`; the long form reports 4292 over 118
with its bottom (422) above the panel's top (430); the pill is at 133–148 above the question's
154; the question is 4 line boxes of 28px — the clamp's ceiling, with no desk breakpoint of its
own; and *when* the card's column is scrolled to its end (288 px), *then* the note is at
635–704, inside the viewport and above `p.keys` at 772–800.

*And when* `s` skips to the `P1` card and the same sweep runs on it,
*then* the same holds with three answers instead of four: the recommended one is at 502–546 at
390, 697–741 at 820 and 450–494 at 1280, each the answers' own width (367, 782, 666 px), the
panel scrolls nothing at any width (559, 499, 499), and the note lands at 680–749, 1016–1085 and
636–705 respectively. Its shorter question is 5, 3 and 3 line boxes.

In all of them, nothing in the card paints under the priority pill: no `.priority` or `.pill`
client rect intersects any `.context` or long-form client rect — the pill is above the question
at every width, so there is no text near it to paint under.

**A16 — a notice does not move the reader inside the card (WEB-35).**
*Given* the `P0` card on a 390 × 844 phone with the reader placed inside it — the card's own
column scrolled to its end (414 px) and the long-form region 60 px down — and the served-markup
snapshot the page diffs against poisoned, so the next projection cannot hand the live node back,
*when* another lane raises a third card through the CLI and the live socket brings the
projection,
*then* the current card's node HAS been replaced, the projection is the one with three cards,
the same item is still on screen, its column is still at 414 px and its long-form region still
at 60 px, and the note field is still at 716–785 — inside the viewport and above `p.keys` at
816. Without the restore the same scenario reports both positions at 0 and the note at
1130–1199, off the screen.

**Categories deliberately not exercised here.** Unauthenticated access, session handling and
CSRF-token design are the edge's and are already proven where they live: kanban implements no
authentication (ADR-016:49), the actor header is trusted only from the edge
(`serve_actor_header_uses_trusted_edge_value_and_refuses_bad_requests`), and POSTs are refused
unless same-origin (`rust/serve.rs:465`). Concurrency and idempotency of a decision are proven
by the shipped `a_picked_verdict_survives_a_live_refresh_and_still_records_in_real_chrome` and
`a_redelivered_notice_does_not_act_or_render_twice_in_real_chrome`; this slice changes neither.

## 5. Contracts and data

**Nothing changes.** This slice alters the stylesheet, the markup emitted by the renderers, and
the copy. It does not touch:

- **HTTP.** The read routes and the four write verbs are unchanged and pinned by WEB-55. No
  status code, form field name (`decision`, `reply`, `outcome`), query parameter (`replied`,
  `undone`, `opened`, `changed`, `show`, `q`) or header changes. There is no OpenAPI document
  for this server and this slice does not introduce one: the interface is server-rendered HTML
  plus four form POSTs, and the project's contract for it is the route table in `render`/`post`
  plus the tests named in §8.
- **The `/live` WebSocket.** Frame shapes (`notice`, `behind`) and keys are unchanged
  (`rust/serve.rs:1285`, `rust/serve.rs:1308`).
- **Board schema and storage.** No migration, no column, no event kind. The card's parts and the
  decision record stay exactly as ADR-042 §1-§3 defines them.
- **Data invariants.** Card order (ADR-042 §5) and the single trusted-edge resolve call site
  (`resolve_attention_from_trusted_edge`, `rust/serve.rs:584`) are unchanged; WEB-55 and the
  shipped `trusted_edge_resolution_stays_on_the_single_web_call_site` prove it.

Because no interface or stored shape moves, there is no migration, no compatibility window and
no ownership transfer to record.

## 6. Quality and security

**Accessibility.** WCAG 2.2 level AA is the target for the restyled surfaces, asserted by
WEB-48, WEB-49, WEB-50, WEB-51, WEB-52, WEB-53 and WEB-59 (contrast, non-text contrast, visible
focus, reduced motion via WEB-20, labelling, the two announcement channels, accessible names).
Conformance is claimed only for what those requirements measure, not as a full-page audit of
every success criterion.

**A separate commitment, above AA: 44 CSS px hit targets.** WEB-45 requires 44 × 44 CSS px on
the deck's controls. AA's own figure is 24 × 24 (SC 2.5.8), so this is not a conformance claim —
it is Apple's Human Interface Guidelines' 44 pt minimum applied to the device George decides on,
under his "use SDD to do the UI/UX using best practices" (2026-09-17). It is recorded here as a
product commitment so that a later reader does not mistake it for the standard's number, and
George may lower it to 24 without touching AA conformance.

**Security — ASVS applicability.** The selected version is OWASP ASVS 5.0.0. Individual ASVS
requirement IDs are **not** claimed: the 5.0.0 text was not reviewed for this slice, so the
applicability below is by chapter theme.

- *Out of scope, with reason:* authentication, session management and access-control policy.
  Kanban implements none of them and there is no flag to change that (ADR-016:49); the edge is
  the Google SSO for `*.geoy.ws`. A specification for the UI's styling cannot and must not
  restate the edge's posture.
- *In scope and already proven by existing tests, unchanged by this slice:*
  - Access control and trusted identity (V4-style):
    `serve_actor_header_uses_trusted_edge_value_and_refuses_bad_requests`,
    `serve_actor_header_defaults_to_geo_when_flag_is_absent`,
    `serve_actor_header_duplicate_cli_flags_fail_closed`,
    `trusted_edge_resolution_stays_on_the_single_web_call_site`,
    `the_trusted_edge_actor_header_records_the_same_actor_over_a_socket`.
  - Validation, encoding and injection (V5-style):
    `markdown_renders_in_real_chrome_and_raw_html_stays_inert`,
    `the_served_pages_read_the_real_boards_and_write_to_none_of_them`,
    `served_board_pages_fail_closed_on_duplicate_names`,
    `serve_hides_retired_boards_from_the_board_index_and_board_route`.
  - Configuration and the listener boundary (V14-style):
    `a_socket_listener_serves_pages_and_the_kernel_holds_the_mode_it_was_given`,
    `serve_refuses_two_listeners_and_no_listener_with_the_usage_status`,
    `the_server_refuses_a_port_it_could_not_be_found_on`,
    `serve_refuses_a_socket_path_that_holds_something_it_must_not_delete`.
- *In scope and asserted by this slice:* WEB-54 (no third-party origin in the document, so the
  edge's `self`-scoped policy keeps applying) and WEB-55 (the write surface does not grow).

**Reliability and performance.** No commitment is made or implied. The inline stylesheet and
script measured 26,213 and 52,211 bytes at the baseline; that is an observation recorded for
future comparison, not a budget, and only George may make it one.

## 7. Open questions

**OQ-1 — `--overlay`'s hex versus the plan's own AA claim.** *Status:* **RESOLVED**
2026-09-17 by codex@driver under George's standing instruction "use SDD to do the UI/UX using
best practices" (2026-09-17). *Gate:* none; this question no longer blocks `SPEC-READY`.
The plan's token table gives `--overlay` `#6c7086` and annotates it "≥ 4.5:1 on base: 4.6:1".
The computed WCAG ratio of `#6c7086` on `#1e1e2e` is **3.36:1**, so the plan's hex failed the
plan's own stated rule. The rule wins: `--overlay` is Catppuccin Mocha `overlay2` `#9399b2`
(5.81:1 on `--base`, 6.22:1 on `--mantle`), the lightest overlay step that satisfies it —
`overlay1` `#7f849c` reaches only 4.44:1. George may overrule by a later note, in which case
WEB-08, WEB-48 and WEB-49 change together; until then this is decided, not pending.

**OQ-2 — borderless answers under SC 1.4.11.** *Status:* Open. *Owner:* George. *Gate:*
ADR-046 acceptance. The plan removes every outline but the focus ring, so an alternative answer
is identified by its `--surface0` fill (1.3:1 against `--base`) and its label (5.4:1 - 9.9:1).
WEB-50 asserts 3:1 only where the plan's own design makes a boundary load-bearing. A stricter
reading of SC 1.4.11 would want a 3:1 boundary on the alternatives, which would reintroduce an
outline the plan deliberately removed. Recorded rather than decided.

**OQ-3 — Tailwind.** *Status:* **DECLINED** (decision recorded in ADR-046 §3; George may
overrule). *Owner:* George. George suggested Tailwind for the revamp. It is declined because it
would put a Node toolchain and a build step between `cargo build` and the served page: today the
whole UI is two `&'static str` constants compiled into the binary (`rust/serve.rs:3747`,
`rust/serve.rs:4799`), the release is one binary whose served exe is proven on install
(`hig_release_script_install_restarts_kanban_serve_and_proves_the_served_exe`), and a
utility-class framework would also be a second vocabulary beside the design tokens this slice
makes normative. If George overrules, ADR-046, this specification and the matrix change together.

**OQ-4 — ADR-046 acceptance.** *Status:* Open. *Owner:* George. *Gate:* ADR-046 moving from
Proposed to Accepted. The decision stands as Proposed until George reviews screenshots of the
restyled deck on the phone and the Mac.

## 8. Verification

Planned test names are proposals; names ending `_in_real_chrome` are compiled-binary real-Chrome
tests in `tests/e2e.rs`, names ending `_over_http` are compiled-binary HTTP tests in the same
target, and names ending `_unit` are `#[test]` functions in `rust/serve.rs`'s `mod tests`. Names
marked † already exist at the baseline and are extended rather than added. The matrix
rows are the rows to add to `docs/testing/compiled-rust-e2e-matrix.md`:

- **M1** — "Web design system: served CSS, markup and copy invariants (spec WEB, unit)".
- **M2** — "Web decision deck: one motion, feedback, toasts and the queue (spec WEB, real
  Chrome)".
- **M3** — "Web navigation, read pages, responsiveness and accessibility (spec WEB, real
  Chrome)".
- **M4** — "Web destinations answer without a script (spec WEB, HTTP)".

| Requirement | Planned test | Layer | Matrix row |
| --- | --- | --- | --- |
| WEB-01 | `the_stylesheet_names_one_serif_and_reserves_mono_for_code_unit` | unit | M1 |
| WEB-02 | `the_question_is_set_as_a_headline_in_real_chrome` †, `the_deck_answers_stack_and_the_note_is_reached_by_one_scroller_at_three_widths_in_real_chrome` | chrome | M2 |
| WEB-03 | `no_heading_or_link_carries_a_glyph_prefix_unit` | unit | M1 |
| WEB-04 | `the_type_scale_is_declared_and_nothing_is_tracked_out_unit` | unit | M1 |
| WEB-05 | `the_stylesheet_names_one_serif_and_reserves_mono_for_code_unit` | unit | M1 |
| WEB-06 | `prose_blocks_are_bounded_to_seventy_characters_unit` | unit | M1 |
| WEB-07 | `the_question_is_set_as_a_headline_in_real_chrome` | chrome | M2 |
| WEB-08 | `the_token_block_is_the_only_place_a_colour_is_written_unit` | unit | M1 |
| WEB-09 | `no_border_or_outline_exists_outside_the_allowlist_unit` | unit | M1 |
| WEB-10 | `only_two_radii_exist_and_each_has_one_job_unit` | unit | M1 |
| WEB-11 | `an_outcome_hue_appears_only_on_an_outcome_unit` | unit | M1 |
| WEB-12 | `the_recommendation_leads_on_fill_in_real_chrome` | chrome | M2 |
| WEB-13 | `nothing_is_boxed_in_real_chrome` | chrome | M2 |
| WEB-14 | `one_motion_is_declared_and_nothing_else_animates_unit` | unit | M1 |
| WEB-15 | `the_advance_runs_once_at_140ms_each_way_in_real_chrome` | chrome | M2 |
| WEB-16 | `a_projection_refresh_never_reanimates_the_current_card_in_real_chrome` | chrome | M2 |
| WEB-17 | `the_pressed_answer_says_sending_on_its_own_fill_in_real_chrome` | chrome | M2 |
| WEB-18 | `the_receipt_lands_with_its_outcome_rule_in_real_chrome` | chrome | M2 |
| WEB-19 | `a_toast_stays_twenty_seconds_and_dismisses_in_real_chrome` | chrome | M2 |
| WEB-20 | `reduced_motion_advances_the_deck_without_animating_in_real_chrome` | chrome | M2 |
| WEB-21 | `the_deck_body_fades_at_its_foot_unit` | unit | M1 |
| WEB-22 | `rendered_meta_is_a_sentence_with_no_dot_chain_unit` | unit | M1 |
| WEB-23 | `the_eyebrow_names_raiser_board_and_age_unit` | unit | M1 |
| WEB-24 | `the_note_field_is_labelled_add_a_note_with_no_hint_unit` | unit | M1 |
| WEB-25 | `the_custom_answer_button_says_record_my_answer_unit` | unit | M1 |
| WEB-26 | `the_empty_deck_copy_and_its_link_are_exact_unit` | unit | M1 |
| WEB-27 | `one_quiet_keys_line_carries_no_kbd_badges_unit` | unit | M1 |
| WEB-28 | `counts_read_as_sentences_unit` | unit | M1 |
| WEB-29 | `a_refusal_is_the_boards_sentence_in_red_unit` | unit | M1 |
| WEB-30 | `pressing_1_sends_and_advances_to_the_next_card_in_real_chrome` † | chrome | M2 |
| WEB-31 | `skip_moves_the_card_to_the_back_without_recording_in_real_chrome` † | chrome | M2 |
| WEB-32 | `the_undo_key_bring_back_the_last_decision_in_real_chrome` † | chrome | M2 |
| WEB-33 | `a_refused_decision_brings_the_card_back_in_real_chrome` † | chrome | M2 |
| WEB-34 | `the_last_card_leaves_the_empty_state_in_real_chrome` † | chrome | M2 |
| WEB-35 | `the_deck_shows_one_card_and_only_its_body_scrolls_in_real_chrome` †, `the_deck_answers_stack_and_the_note_is_reached_by_one_scroller_at_three_widths_in_real_chrome`, `a_projection_swap_keeps_the_reader_where_they_were_in_the_card_in_real_chrome` | chrome | M2 |
| WEB-36 | `the_drawer_is_rows_on_the_desk_surface_in_real_chrome` | chrome | M3 |
| WEB-37 | `the_current_page_is_marked_by_a_rule_in_real_chrome` | chrome | M3 |
| WEB-38 | `every_destination_answers_without_a_script_over_http` | http | M4 |
| WEB-39 | `the_drawer_puts_search_before_every_destination_unit` | unit | M1 |
| WEB-40 | `read_pages_are_rows_with_one_pill_and_a_mono_priority_in_real_chrome` | chrome | M3 |
| WEB-41 | `exactly_one_pill_style_exists_unit` | unit | M1 |
| WEB-42 | `tables_declare_only_the_row_hairline_unit` | unit | M1 |
| WEB-43 | `read_tables_are_borderless_but_for_the_hairline_in_real_chrome` | chrome | M3 |
| WEB-44 | `no_route_overflows_sideways_at_three_widths_in_real_chrome` | chrome | M3 |
| WEB-45 | `every_deck_control_is_forty_four_pixels_at_390_in_real_chrome` | chrome | M3 |
| WEB-46 | `the_desk_is_two_columns_at_1280_and_a_drawer_at_820_in_real_chrome` | chrome | M3 |
| WEB-47 | `every_answer_gets_its_own_row_and_the_panel_scrolls_nothing_unit` | unit | M1 |
| WEB-48 | `every_token_pair_clears_four_and_a_half_to_one_unit` | unit | M1 |
| WEB-49 | `overlay_never_sits_on_surface0_unit` | unit | M1 |
| WEB-50 | `the_focus_ring_and_the_filled_answer_clear_three_to_one_unit` | unit | M1 |
| WEB-51 | `focus_is_visible_on_every_focusable_element_in_real_chrome` | chrome | M3 |
| WEB-52 | `every_field_is_labelled_and_status_is_announced_once_unit` | unit | M1 |
| WEB-53 | `the_card_is_named_by_its_question_in_real_chrome` | chrome | M2 |
| WEB-54 | `the_document_references_no_third_party_unit` | unit | M1 |
| WEB-55 | `the_route_table_and_the_write_allowlist_are_unchanged_unit` | unit | M1 |
| WEB-56 | `the_open_page_without_a_script_is_still_a_list_in_real_chrome_or_http` † | chrome | M2 |
| WEB-57 | `an_incomplete_own_answer_refuses_before_posting_in_real_chrome` | chrome | M2 |
| WEB-58 | `every_deck_rule_is_scoped_to_a_page_whose_script_ran` † | unit | M1 |
| WEB-59 | `the_live_line_and_the_toast_log_say_only_their_own_thing_in_real_chrome` | chrome | M2 |

Counts: 59 requirements — 57 MUST, 2 SHOULD (WEB-06, WEB-21), no MAY; by layer, 30
`unit`, 28 `chrome`, 1 `http`. WEB-47's MAY became a MUST on 2026-09-18, and
WEB-40 moved from `unit` to `chrome` with `t-bf255880` wave 1, when every page
that lists rows became the bundle's.

## Appendix A — the design plan, verbatim

Reproduced byte-for-byte from `/private/tmp/web-design-plan.md` (codex@driver, 2026-09-17). The
plan text below is unaltered; the one editor's note is here, above it, rather than inside it.

**Editor's note on the token table below (§7 OQ-1, resolved 2026-09-17):** the plan's
`--overlay` `#6c7086` measures 3.36:1 on `--base`, not the "4.6:1" written beside it, so the
normative token is Catppuccin Mocha `overlay2` `#9399b2` (5.81:1); everything else in the plan
stands as written.

---

# Kanban web — design plan (codex@driver, 2026-09-17, under /frontend-design)

## Subject, audience, job

The subject is a **decision desk for one person who runs many agents**. Agents raise questions into a hash-chained work ledger; George answers them from an iPhone or a Mac, one at a time, and needs to see that each answer landed. Everything else the site shows — boards, lanes, sprints, plans, deployments, subscriptions, search, task pages — is the ledger being read, never edited (ADR-016: the only browser writes are decide, undo, plan-open, subscription pause/resume).

Audience: George, alone, signed in through Google SSO. Devices: iPhone (Safari, 390 wide) and Mac (Chrome/Safari, 1280+). Terminal person; `kb` is a CLI first.

Primary job: **answer the next question fast and unmistakably, and know it was recorded.** Secondary: read what the estate is doing without knowing which board owns a fact.

## What is wrong with the current look (critique of 56e6b24, screenshots 2026-09-17)

- SaaS-card kit: every unit is an outlined rounded box on a dark page — the card, each choice, the note field, the history rows, the nav. Same 6px radius on everything regardless of hierarchy; nothing is *the* thing.
- Meta strings joined with middle dots (`LOOK · review · asked by codex@driver-2 · waiting just now`) above every heading — the tracked-out eyebrow tell.
- The question is 1.35rem with a `> ` glyph in front: it reads as a log line, not as a question put to a person.
- Two buttons of equal weight, both outlined, tinted by outcome hue with low contrast against the base — the recommendation does not lead.
- A separate keys line with eight `<kbd>` badges competes with the answers.
- Motion is scattered: card slide-in, receipt flash, toast fade, and the projection swap re-runs the slide-in on the same card whenever a notice arrives (George: "it keeps blinking").
- Toast at 6 s is unreadable (George: "the toast is too fast").

## Tokens

### Color — Catppuccin Mocha (George's OMP theme), used with hierarchy, not as tint

| name | hex | role |
|---|---|---|
| `--base` | `#1e1e2e` | the page |
| `--mantle` | `#181825` | the answer panel, the side column, the drawer — the "desk" surfaces |
| `--surface0` | `#313244` | fills: alternative buttons, inputs, history rows, pills |
| `--surface1` | `#45475a` | hover/active fill; the one hairline the design keeps (between body and panel) |
| `--text` | `#cdd6f4` | headline and body |
| `--subtext` | `#a6adc8` | secondary prose: context, consequences, notes |
| `--overlay` | `#6c7086` | quiet meta sentences, timestamps, hints (≥ 4.5:1 on base: 4.6:1) |
| outcomes | approve `#a6e3a1` green · reject `#f38ba8` red · defer `#f9e2af` yellow · other `#89b4fa` blue | the only accents; a colour means an outcome, nothing else |
| `--link` | `#89b4fa` | links; same blue as "other" is acceptable because links never sit inside an answer button |
| `--focus` | `#b4befe` lavender | focus ring only |

No outlines except the focus ring. Fill, size and type carry hierarchy.

### Type — two families, clearly distinct, no downloads (CSP self)

- **Question / page titles:** `ui-serif, 'New York', 'Iowan Old Style', Charter, Georgia, serif` — semibold 600. This is the one memorable element: a question set like a headline, in a face that reads as *written to a person*, on a dark terminal-coloured desk.
- **Everything else:** `-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif` — 400/500/600.
- **IDs, SHAs, commands, keys:** `ui-monospace, SFMono-Regular, Menlo, monospace` — only for things that are literally code.

Scale (rem, line-height): question 1.75/1.15 phone → 2.25/1.1 ≥ 900px; page h1 1.5/1.2; section h2 1.0625/1.3 weight 600 in `--subtext` (sentence case, no caps, no `> ` glyph); body 1/1.55; meta .8125/1.4; button label 1/1.2 weight 600; digit badge .75 mono. Line length ≤ 70ch for prose.

### Layout

Deck at 390 (the whole viewport is the card; nothing outlines it):

```
┌──────────────────────────────────────┐
│ ≡                        3 left  ⏱1  │  bar: menu · remaining · history
│                                      │
│ codex@driver asked on px, 3 days ago │  overlay, one sentence
│                                      │
│ Ship the sprint tonight,             │  serif 1.75 semibold, text
│ or wait for Monday?                  │
│                                      │
│ Gate green at 353/0. Rollback is one │  subtext 1rem — context
│ command. Waiting costs a working day.│
│ ┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈│  (no rule — the body simply continues)
│ The long form, markdown, scrolls     │  flex:1, overflow-y:auto,
│ here and only here …                 │  bottom fade 2rem to signal more
├══════════════════════════════════════┤  panel: mantle, sticky bottom
│ ┌──────────────────────────────────┐ │
│ │ ① Ship tonight                   │ │  filled green, base-coloured text
│ └──────────────────────────────────┘ │
│ The release goes out now; rollback   │  subtext, consequence of ①
│ stays one command.                   │
│ ┌───────────────┐ ┌────────────────┐ │
│ │ ② Wait        │ │ ③ Something    │ │  surface0 fill, label in outcome hue
│ └───────────────┘ └────────────────┘ │  (2-up; a 3rd/4th alternative wraps)
│ Add a note                           │  surface0 field, one line, grows to 4
│ Answer in my own words ▾             │  fold (details), overlay text
│ 1–4 answer · s skip · u undo · c own │  ONE quiet line, .75rem overlay, mono
└──────────────────────────────────────┘
```

Deck ≥ 900px: deck column `max-width:44rem`, left-aligned within the remaining width with 3rem inset; side column `22rem` fixed right on mantle holding toasts (top) and "Decided this session" (below). Between 600 and 900 the side column is a right drawer behind the history button, as today.

Every other page (`/all`, `/decided`, boards, lanes, sprints, plans, deployments, subscriptions, search, task, epic, story, deployment): the **row** is the unit, not the card.

```
Board name                                   h1 serif 1.5
One sentence of what this page lists.        overlay

Title of the row, text, weight 500           ← link if it opens a page
codex@driver · in progress · P1 · 2h ago     ← NO: replaced by a sentence:
codex@driver started it 2 hours ago, P1      overlay .8125rem
──────────────────────────────────────       one hairline surface0 between rows
```

Status is categorical, so it keeps a pill: `surface0` fill, `.75rem`, text in the status hue (`done` green, `in_progress` blue, `blocked` red, `review` yellow, `todo`/`backlog`/`draft` overlay). One pill style everywhere. Priority: `P0`/`P1`/`P2` as plain mono text in overlay; P0 alone in red.

Tables (sprints, deployments, receipts) stay tables but lose borders: header row in overlay .75rem, rows separated by the one hairline, numeric columns right-aligned in mono.

Navigation: hamburger + drawer as shipped, restyled: drawer on mantle, links as rows with the same hairline, current page marked by a 2px left rule in `--text` (not a filled block), search field at the top of the drawer.

### Motion — one moment

The **card advance** is the only non-user-triggered motion: out `translateX(-16px)`+fade 140 ms, in from `+16px` 140 ms. Nothing else animates: no hover transitions, no receipt flash, no toast fade — a toast appears, a history row appears. The projection refresh MUST NOT re-run the advance on a card that is still the current card (diff by `data-item`; replace only nodes whose markup changed; never touch the current card node while unchanged). `prefers-reduced-motion` removes the advance too.

Feedback: the pressed answer reads **Sending…** on its own fill (no disabled grey — reduce opacity of the *other* answers instead), then the history row lands with a 3px left rule in its outcome hue. Toast: stays **20 s**, pauses while hovered or focused, dismisses on click/Esc, at most three stacked, newest on top.

## Copy

- Meta is a sentence, never a pipe or dot chain: "codex@driver asked on px, 3 days ago"; "claude@driver-2 started this 2 hours ago"; "decided by geoyws just now".
- Buttons say what happens: the choice label as authored; the custom one is "Record my answer".
- The note field's label is "Add a note"; the hint under it is gone (the fold's summary explains the other path).
- Empty deck: "Nothing is waiting. Every question an agent raised has an answer." with a link "See what was decided".
- Refusal: the board's own sentence, in `--red` text on the panel, no box.
- Counts: "3 left" on the deck bar; "12 open across 4 boards" on `/all`.

## Principles (what makes this page this page)

1. **The question is the hero.** It is the largest thing on the screen and the only serif.
2. **Surfaces, not boxes.** Two desk surfaces (base, mantle); hierarchy by fill and type; one hairline; no outlines but focus.
3. **A colour is an outcome.** Green/red/yellow/blue mean approve/reject/defer/other everywhere, and nothing else is coloured.
4. **One motion.** The advance; everything else just appears.
5. **Sentences, not chains.** Meta reads as prose; no dot-joined strings, no caps labels, no glyph prefixes.
6. **The keyboard is quiet.** One hint line; the digits live on the buttons.

## Review against the generic defaults (pass 2)

- Trait 2 (near-black + one acid accent): avoided — Mocha base is blue-violet dark, and there are four semantic accents, none decorative.
- Trait 4 (card kit): explicitly removed — rows and surfaces replace boxes; one radius only on buttons/inputs (8px) and pills (999px).
- Trait 5 (eyebrow caps, middle dots, `>` prefixes, mono for labels, `→` on links): all removed by the copy rules; mono is reserved for literal code/IDs.
- Serif display on dark: this is the deliberate bold choice, spent in one place; on cream it would be the trait-1 tell, on Mocha with system New York it is specific to George's devices and to "a question asked of a person".
- Motion: one orchestrated moment; hover transitions removed.
- What I changed after this review: dropped the planned 3px outcome-coloured left rule on *every* row of the list pages (it was decoration); kept it only on history rows, where it encodes the outcome. Dropped a planned "P0" red pill in favour of plain red mono text, so red keeps meaning "reject/blocked/urgent" and not "a pill".
