# Specification: the operator UI is one embedded TypeScript application (slice SPA)

## 1. Identity and baseline

- **Slice ID:** `SPA`. Requirement IDs are `SPA-01` .. `SPA-57`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-09-19` at commit `e3ae94a` on branch `docs/t-eed0a923-spa-spec`. Every
  "today" claim below cites the line that has it, as `<path>:<line>`.
- **Status:** `SPEC-READY` on 2026-09-19 (independent gate by a reviewer applying the SDD §1 exit criteria over three rounds: eleven findings, all closed; specification readiness only - it authorises neither implementation nor rollout nor release).
  <!-- SPEC-READY is stamped here by an independent reviewer against the SDD reference's §1 exit
  criteria, not by the writer of this document. It authorises neither implementation nor rollout
  nor release. -->
- **Owner (product scope):** George. He alone resolves scope, the first-wave boundary, and
  whether a retired behaviour is reinstated.
- **Decider (wording of this document):** the `t-eed0a923` writer.
- **Sources:**
  - `docs/adr/ADR-048-the-operator-ui-is-a-typescript-spa-embedded-in-the-binary.md` — the
    decision this specification implements. §1 (one executable, embedded bundle, nothing
    Node-shaped on `hax` or `hig`), §5 (the JSON API is a thin projection over the same `Store`,
    or it is not built) and §6 (the accepted costs) are binding on every requirement below.
  - `docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md` — the server that is amended, not
    replaced: the `Store` call rule, loopback only, the trusted edge, the write allowlist, the
    `/live` frame-content rule.
  - `docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md` §1, §3, §5 —
    the decision card, the composer that derives the resolution, and the card order.
  - `docs/adr/ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md`
    §2 — measured build provenance, which the bundle's fingerprint rides in.
  - `docs/adr/ADR-046-the-web-ui-is-one-designed-system.md` — the design system the SPA wears;
    this specification preserves it by citation, never by copy.
  - `docs/adr/ADR-047-kanban-adopts-specification-driven-development.md` — the conventions this
    document is written under; §9 is why the payload number below is an observation and not a
    budget.
  - `docs/specs/web-ui.md` `WEB-01` .. `WEB-59` — the observed baseline. Every behavioural
    obligation that survives the rebuild is carried here as an `SPA-nn` naming the `WEB-nn` it
    preserves; §3's "What is retired" group names the two that do not survive.
  - Shipped surface at the baseline: `rust/serve.rs:400`-`rust/serve.rs:418` (`ROUTE_SHAPES`, the
    registry of every page arm), `rust/serve.rs:421`-`rust/serve.rs:470` (`render`),
    `rust/serve.rs:472`-`rust/serve.rs:491` (`post`'s four-verb allowlist),
    `rust/serve.rs:492`-`rust/serve.rs:500` (the same-origin refusal),
    `rust/serve.rs:116`-`rust/serve.rs:118` (`actor_for_write`, defaulting to `OPERATOR_ACTOR`),
    `rust/serve.rs:211`-`rust/serve.rs:222` (`accept`: `/live` is upgraded before routing),
    `rust/serve.rs:1096`, `rust/serve.rs:1121`, `rust/serve.rs:1131`, `rust/serve.rs:1142`
    (the four frame kinds: `ready` with the revision fingerprint, the scan's notices, `refresh`
    with the new fingerprint, `heartbeat`), `rust/serve.rs:3865` (`JS`), `rust/serve.rs:5103`
    (`CSS`), `rust/store.rs:6094`-`rust/store.rs:6098` and `rust/model.rs:1292`-`rust/model.rs:1298`
    (the two board refusals a concurrent rewrite produces).
  - The harness seam, landed ahead of this slice: `tests/e2e.rs:26171` (`mod ui`),
    `tests/e2e.rs:26177` (`SHELL`), `tests/e2e.rs:26181` (`APP_ROOT`, "Not served yet — the mount
    is t-eed0a923's"), `tests/e2e.rs:26324` (`wait_for_app_root`), `tests/e2e.rs:26362`
    (`wait_for_shell_ready`, whose comment says it "becomes `wait_for_app_root(tab,
    ui::APP_ROOT)` and nothing else in the file has to know"), `tests/e2e.rs:26480`
    (`an_async_mounted_root_is_found_by_test_id_in_real_chrome`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`. §8 carries the slice's evidence
  table; the matrix section is the trace of record and the two are kept identical by the same
  change.

## 2. Purpose and scope

**Intended outcome.** `kb.geoy.ws` keeps doing the one job the deck exists for — answer the next
question fast and unmistakably, and know it was recorded — while the client that does it becomes
a React + TypeScript application built once at build time and embedded in the same single Rust
executable, fed by a read-only JSON projection over the same `Store` methods the CLI calls.

**Users / actors.**

- George, alone, signed in through the Google SSO at the edge, on an iPhone (Safari, 390 CSS px
  wide) and on a Mac (Chrome/Safari, 1280+). He is the only writer.
- Agent lanes, which never open the browser: they raise and read through the CLI and MCP, and
  their writes reach the operator as `/live` notices.
- The nginx edge, which authenticates and sets the actor header; kanban implements no
  authentication of its own (ADR-016).
- The build host, which compiles the bundle and hands it to `cargo build` as bytes. It is not a
  runtime actor: nothing of its shape runs on `hax` or `hig` (ADR-048 §1).

**In scope.** Every page `render` serves at the baseline — `/`, `/all`, `/decided`, `/boards`,
`/sprints`, `/sprints/{project}`, `/sprint/{project}/{id}`, `/plans`, `/deployments`,
`/subscriptions`, `/lanes`, `/search`, `/preview/{task|attention|deployment}/{project}/{id}`,
`/preview/board/{project}`, `/board/{project}`, `/task/{project}/{id}`,
`/deployment/{project}/{id}` and the not-found arm (`rust/serve.rs:400`-`rust/serve.rs:418`) —
the four write verbs `post` allows (`rust/serve.rs:480`-`rust/serve.rs:486`), the `/live`
WebSocket (`rust/serve.rs:213`), the read-only JSON projection that feeds the pages, the embedded
bundle and the mounted application root.

**Boundaries.**

- The `Store` and its authorization are owned by the ledger, not by this slice. The projection
  calls them; it never reaches past them (ADR-048 §5).
- The edge owns authentication, sessions and public reachability. A specification for the
  operator UI cannot restate the edge's posture and does not try.
- The release receipt's schema is ADR-039's and ADR-044's; this slice states what the receipt
  must now prove, and `t-992e40aa` decides where the field sits (§7 OQ-4).
- The bundler and the React version are `t-992e40aa`'s to choose within §1's constraint (§7
  OQ-1). ADR-048's Consequences deliberately left them open.

**Non-goals.**

- **No React Native and no App Store client.** Raised by George on 2026-09-17 and withdrawn by
  him the same day; ADR-048 §4(c) records the two gates — real authentication in a ledger that
  has none, and public reachability for a loopback-bound tool — that anything mobile-native
  would have to clear first.
- **No multi-user identity.** No accounts, no sessions, no tokens. There is one operator and one
  trusted-edge actor header.
- **No second read path past `Store`.** No SQL written for the browser that the CLI does not
  already run, and no second query implementation (ADR-048 §5).
- **No new write endpoint.** The browser's writes stay exactly the four verbs in `post`'s
  allowlist: decide, reopen (undo), plan-open, and subscription pause/resume.
- **No invented performance or availability commitment.** §6 records one measurement as an
  observation. Only George may turn it into a budget (ADR-047 §9).

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes, not a browser.
- `http` — a compiled-binary HTTP exchange against `kanban serve`, no browser.
- `chrome` — a compiled-binary end-to-end test driving real Chrome.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

IDs are assigned in creation order and never reused; the groups below are topical. A requirement
that preserves an obligation `docs/specs/web-ui.md` already pins names that `WEB-nn` in its
Source line, so a reviewer can read the two documents side by side and see what moved and what
merely changed mechanism. ADR-048 §6 is explicit that WEB's real-Chrome contracts are
**re-proven on the SPA**, not assumed to carry over: every such requirement is a fresh
obligation on a new artefact that happens to have the same observable.

### Identity of the artefact

**SPA-01** — one executable serves the operator UI from its own bytes.
Strength: MUST · Layer: process · Source: ADR-048 §1.
The application's HTML, JavaScript and CSS are produced at build time and compiled into the one
`kanban` executable, exactly as `JS` (`rust/serve.rs:3865`) and `CSS` (`rust/serve.rs:5103`) are
today. A running `kanban serve` serves every asset the mounted page needs from that executable's
own bytes: with the build tree, the source tree and the network removed, a fresh process started
from the installed binary alone still serves a page that mounts. No asset is read from the
filesystem at request time and none is fetched from a third party.
*Data rules:* nothing about the bundle is written to the board; the bundle is program text, not
ledger state.

**SPA-02** — nothing Node-shaped runs on the serving hosts, or is needed to run what they serve.
Strength: MUST · Layer: process · Source: ADR-048 §1; rule `r-cf2b2b9f` scoped to `RUNTIME` with
the build-time exception ADR-047's Consequences record.
On `hax` and `hig` there is no Node, Bun, npm, pnpm, Yarn or Corepack process, no such runtime
dependency in the installed release package, and none is required to start `kanban serve` or to
render any in-scope route. The build-time exception is exactly that: a toolchain on a build host,
whose only output crossing to a serving host is bytes inside the executable.
*Failure behaviour:* a release package that carries a Node-shaped runtime, or an executable that
execs one, fails this requirement; the correct response is to fail the release, not to install
the runtime.

**SPA-03** — the bundle's fingerprint is half of the release proof.
Strength: MUST · Layer: process · Source: ADR-048 §6 (the single-artifact release proof gains a
second half); ADR-044 §2.
A deploy receipt for a release carrying this UI records a fingerprint of the embedded bundle
alongside the executable identity the current practice proves, and the fingerprint recorded is
the one the running process actually serves. `readlink /proc/<MainPID>/exe` resolving inside the
activated `releases/<id>` is necessary and no longer sufficient. An embedded bundle whose
provenance is not in the receipt is an unproven artefact and the deploy is not complete.
*Data rules:* the fingerprint is derived from the built bytes, not typed by a human, and it is
durable on the receipt. Where the field sits in the ADR-044 §2 provenance record is §7 OQ-4.

### Readiness

**SPA-04** — the mounted root announces itself by a stable test id.
Strength: MUST · Layer: chrome · Source: ADR-048 Consequences (the landed seam);
`tests/e2e.rs:26181`.
Every in-scope route mounts one element carrying `data-testid="app-root"`, it is visible
(`checkVisibility()` is true) once the application has mounted, and it is the element
`wait_for_app_root(tab, ui::APP_ROOT)` returns. The id is stable across routes and across
re-renders: navigating within the application does not unmount and remount a differently named
root.

**SPA-05** — the shell is not the page.
Strength: MUST · Layer: chrome · Source: ADR-048 Consequences; `tests/e2e.rs:26354`-`:26365`.
Readiness is the mounted root of SPA-04 and never the first response's markup. The first response
may contain a shell element, but a test that waits only for the shell MUST NOT be able to observe
the application's content: at the instant the shell resolves, the page's own data-bearing
elements — the current card, a list's rows, a table's cells — are not yet required to exist.
*Failure behaviour:* a page that mounts and then throws leaves no visible root, and the harness
reports "no visible `[data-testid=app-root]` was mounted", naming the mount rather than timing
out on an unrelated selector.

### The JSON projection

**SPA-06** — every page's data arrives as JSON over a read-only route.
Strength: MUST · Layer: http · Source: ADR-048 §1, §5.
For each in-scope page arm of `render` (`rust/serve.rs:429`-`rust/serve.rs:468`) there is a
`GET`-only JSON route that returns that page's data, and the mounted application obtains its data
from those routes and from `/live` and from nothing else. A JSON route answers `GET` and refuses
every other method; no state is changed by reading one, and two identical reads of an unchanged
board return equal bodies.
*Permissions:* the reader is the same trusted-edge principal the page is served to; the route
carries no capability of its own.
*Open:* the naming and versioning of these routes is **not decided here** — §7 OQ-3 owns it, and
`t-19d4c16d` is the row that answers it. This requirement pins that a route exists per page arm
and what it may do, not what it is called.

**SPA-07** — the projection's only data dependency is `Store`.
Strength: MUST · Layer: unit · Source: ADR-048 §5 (binding); ADR-016's own `Store` rule.
The module that serves the JSON projection imports neither `rusqlite` nor any SQL helper, holds
no `SELECT`, `INSERT`, `UPDATE` or `DELETE` string, and every handler in it takes `&Store` (or
the read guard) and obtains its data by calling the `Store` methods the page arm it replaces
calls today. A handler with its own SQL fails this requirement even if its output is
byte-identical to the store's.
*Failure behaviour:* a page that needs data no `Store` method exposes is a request for a `Store`
method, decided on the ledger's terms, not a licence to write a query in the web layer.

**SPA-08** — authorization is the CLI's, enforced where the CLI enforces it.
Strength: MUST · Layer: http · Source: ADR-048 §5.
A JSON route returns exactly what the same principal would be shown through the CLI: the same
board and tag authorization, enforced inside the store rather than by a filter in the route. A
board the principal may not read is absent from every JSON body, not merely hidden by the client;
a retired board stays absent from the board index and from its own route.
*Failure behaviour:* an unauthorized read is refused or empty by the store's own rule, and the
route adds no second rule of its own.

**SPA-09** — a lease token is never serialised to the browser.
Strength: MUST · Layer: http · Source: ADR-048 §5 ("The API **never** serves a lease token").
No JSON body served to the browser contains a lease token value or the field name `leaseToken`,
on any route, for any principal, in any state — including a task that is currently leased and a
subscription row holding `lease_token`. The same rule holds for every `/live` frame (SPA-49).
*Data rules:* the browser never receives a credential, a cookie value, a secret reference's
value, or any capability it could replay against the CLI.

**SPA-10** — the write surface stays exactly four verbs.
Strength: MUST · Layer: unit · Source: ADR-048 §5; ADR-016's write allowlist; preserves `WEB-55`.
`post`'s allowlist is exactly `attention/{project}/{id}/reply`,
`attention/{project}/{id}/reopen`, `plan/{project}/{id}/open` and
`subscription/{project}/{id}/{pause|resume}` (`rust/serve.rs:480`-`rust/serve.rs:486`). The JSON
surface adds no fifth write, and no `GET` route mutates. A verb added or removed fails this
requirement.

**SPA-11** — a write that did not come from this site is refused.
Strength: MUST · Layer: unit · Source: ADR-048 §5; ADR-016's same-origin gate;
`rust/serve.rs:492`-`rust/serve.rs:500`.
Each of the four writes is refused with `403` unless the request's `Origin` authority equals its
`Host`. The refusal renders the product's own sentence — `The action did not come from this site.`
— and the board is unchanged: no event, no decision, no status transition. The gate is CSRF
defence and grants no capability of its own; it is not weakened, widened or moved by the
introduction of the JSON surface, and a JSON-shaped request body does not exempt a write from it.

**SPA-12** — the actor on a write is the trusted-edge actor.
Strength: MUST · Layer: http · Source: ADR-048 §5; ADR-016's actor clause;
`rust/serve.rs:116`-`rust/serve.rs:118`.
The actor recorded for a browser write is `OPERATOR_ACTOR` (`geoyws`) by default, or the
validated `X-Auth-Request-Email` value when `kanban serve --actor-header NAME` was given. A
missing, duplicated, malformed or oversized header fails closed; a client-supplied actor in a
JSON body, a query parameter or any other field is ignored and can never change the recorded
actor. The trusted-edge resolve call site stays single.

**SPA-13** — the read surface writes nothing.
Strength: MUST · Layer: http · Source: ADR-048 §5; preserves `WEB-55`'s other half.
No page render and no JSON projection can reach a `Store` method that takes `&mut self` outside
the four-verb allowlist. Serving every in-scope route against real boards leaves those boards
byte-identical.

### The Needs-you deck

Every requirement in this group is an obligation `docs/specs/web-ui.md` already pins on the
server-rendered deck and that survives the rebuild as **behaviour**, re-proven on the new
artefact (ADR-048 §6). Each names the `WEB-nn` it preserves and, in §8, the Chrome test that
observes it today.

**SPA-14** — one click on the recommendation records the decision, in place.
Strength: MUST · Layer: chrome · Source: preserves `WEB-30`, `WEB-17`, `WEB-18`; ADR-042 §5.
A single click on the recommended answer, or the digit on it, records that decision on the board
through `attention/{project}/{id}/reply` and leaves a receipt on screen carrying the outcome just
recorded. While the POST is open the pressed control reads `Sending…` on its own fill, is not
`[disabled]`, and every other answer dims; no second click is needed and no page load intervenes.
*Data rules:* the board row moves to `resolved` with the decision the operator picked, and the
note, when one was typed, is recorded with it.

**SPA-15** — a receipt survives a projection swap, so Undo stays reachable.
Strength: MUST · Layer: chrome · Source: preserves `WEB-18`, `WEB-32`.
A receipt rendered for a decision made in this sitting is still on screen, with its outcome and
its undo affordance, after a `/live`-driven projection swap replaces the page's data. A refresh
that discards the session's receipts — and with them the only route back to a decision just
made — fails this requirement.

**SPA-16** — a live refresh never discards typed reply text.
Strength: MUST · Layer: chrome · Source: preserves `WEB-33`, `WEB-35`; ADR-048 §5 (the
in-flight-answer hold is not superseded).
Text the operator has typed into a reply or note field is still in that field, character for
character, after a projection arrives and re-renders — including a projection that replaces the
node the field was mounted on, and including one that arrives while the text is being typed. The
caret's field keeps focus.

**SPA-17** — a picked verdict survives a live refresh and still records.
Strength: MUST · Layer: chrome · Source: preserves `WEB-33`; ADR-042 §1.
A verdict selected in the custom-answer picker is still selected after a projection arrives, and
submitting afterwards records that verdict on the board. A refresh that silently clears the
picked verdict, or that leaves it shown but records a different one, fails this requirement.

**SPA-18** — a decision in flight is never clobbered by a refresh.
Strength: MUST · Layer: chrome · Source: preserves `WEB-17`; ADR-048 §5.
While a decision POST is open, an arriving projection does not replace the card that is sending,
does not re-enable or relabel its pressed control, and does not advance the deck past it. The
answer the board gives is what resolves the card; a notice arriving first changes nothing about
it.

**SPA-19** — a free-text answer needs both halves, and the control stays live.
Strength: MUST · Layer: chrome · Source: preserves `WEB-57`; ADR-042 §1.
Submitting the own-words answer with only one half — a reply with no verdict, or a verdict with
no reply — refuses **before** any POST is made, renders one fixed sentence in its own refusal
channel, moves focus to the missing half, and records nothing: the item stays `open` with no
`decision`. The refusal never names which half is missing. The submit control is left usable
immediately: supplying the missing half and submitting again records the answer without a reload.

**SPA-20** — a complete free-text answer records its outcome.
Strength: MUST · Layer: chrome · Source: preserves `WEB-25`; ADR-042 §1, §3.
An own-words answer with both halves records a decision whose outcome is the verdict picked and
whose reply is the text typed, derived by the store's composer and by nothing in the client.
Committing from the keyboard inside the picker records the same answer as pressing the submit
control.

**SPA-21** — a board refusal is shown verbatim and the pressed control comes back.
Strength: MUST · Layer: chrome · Source: preserves `WEB-29`, `WEB-33`.
When the board refuses a write, the refusal channel carries the board's own sentence verbatim —
no paraphrase, no prefix, no generic fallback — the refused card is the card on screen again at
its own queue position, every answer control on it is enabled and usable, the deck's remaining
count is unchanged, no receipt appears, and any reply text typed before the click is still in its
field. Further typing after the refusal does not clear the refusal or the draft.
*Data rules:* the board row is untouched — `status` stays `open`, `decision` stays absent.

**SPA-22** — a refusal names the card that changed underneath.
Strength: MUST · Layer: chrome · Source: preserves `WEB-29`; `rust/store.rs:6094`-`rust/store.rs:6098`,
`rust/model.rs:1292`-`rust/model.rs:1298`.
When the item was resolved or its authored choices were rewritten between the card being rendered
and the answer being sent, the write is refused by the store and the operator reads the store's
own sentence — `attention {id} was already resolved by {who} — it is history, not a queue entry`,
or `attention {id} has no choice {key}; its choices are {keys}` — through the SPA-21 channel, not
a client-invented message and not a silent re-render. The refreshed card then shows the choices
that now exist, and answering one of those records normally.
*Note:* no test observes this today — both shipped refusal cases stub a `409` at `window.fetch`
rather than letting the board refuse. §8 records it as `none`.

**SPA-23** — a digit outside the answers records nothing.
Strength: MUST · Layer: chrome · Source: preserves `WEB-27`.
A digit typed while the verdict picker holds focus, while focus has moved off a picked verdict,
or while the folded answers summary holds focus, records no decision and does not advance the
deck. The digit shortcuts belong to the answer buttons and to nothing else.

**SPA-24** — a held verdict can always be released.
Strength: MUST · Layer: chrome · Source: preserves `WEB-57`; ADR-042 §1.
`Escape` and the picker's own visible release control each clear a held verdict, leaving the
composer with no verdict selected and the reply text untouched. A hold nobody can release is a
frozen page, so a state with no release path fails this requirement.

**SPA-25** — the notice socket lags, reconnects and redelivers without lying.
Strength: MUST · Layer: chrome · Source: preserves `WEB-16`, `WEB-19`; ADR-016's `/live` clause.
A socket that falls behind a burst of board changes surfaces one summary rather than one notice
per change; a socket that reconnects does not replay history it already delivered; and a notice
delivered twice neither acts twice nor renders twice. Through all three, the page's data still
converges on what the board actually holds.

**SPA-26** — skip moves a card to the back and records nothing.
Strength: MUST · Layer: chrome · Source: preserves `WEB-31`.
Skipping makes the next card current, leaves the skipped card last in the queue, and leaves the
board's attention row `open` with no `decision`.

**SPA-27** — undo brings the last decision back.
Strength: MUST · Layer: chrome · Source: preserves `WEB-32`; ADR-016's reopen verb.
Undoing a decision made in this sitting reopens that item on the board through
`attention/{project}/{id}/reopen` and returns it to the deck as the current card.

**SPA-28** — the last card leaves an answer, not an absence.
Strength: MUST · Layer: chrome · Source: preserves `WEB-26`, `WEB-34`.
Answering the only remaining card leaves no card element in the deck and renders the empty-deck
copy with its link to `/decided`.

**SPA-29** — one card, one scroller, one column of answers, recommendation in the first viewport.
Strength: MUST · Layer: chrome · Source: preserves `WEB-02`, `WEB-35`, `WEB-47`.
At 390, 820 and 1280 CSS px wide, on a card at the product's bounds: the document itself does not
scroll; the card's own column is the one scroller that carries the card's overflow and the answer
panel is not a nested scroller; no two answer buttons share a row and each is the answers' own
content width; the recommended answer's box is fully inside the viewport on the screen the card
opens on; and scrolling the card's column to its end puts the note field fully inside the
viewport, above the keyboard line.

**SPA-30** — a projection swap keeps the reader where they were.
Strength: MUST · Layer: chrome · Source: preserves `WEB-35`.
When a projection replaces the current card's node while the same item is still on screen, both
scroll positions — the card's own column and the long-form region inside it — are restored, and
the note field stays where the reader left it rather than jumping back into a position off the
screen.

**SPA-31** — one quiet keyboard line, and the digits live on the buttons.
Strength: MUST · Layer: chrome · Source: preserves `WEB-27`.
The deck renders exactly one keyboard hint line naming the answer digits, skip, undo and the
own-words fold, in the quiet register; each answer button carries its own digit; and no separate
key-badge cluster competes with the answers.
*Note:* the shipped evidence for the rendered line is a `unit` test over served bytes, which the
SPA retires with the server-rendered markup. §8 records the browser row as `none`.

**SPA-32** — a toast is readable and dismissible.
Strength: MUST · Layer: chrome · Source: preserves `WEB-19`.
A toast stays on screen long enough to read (the shipped figure is 20 s), hovering or focusing it
holds it there, a click and `Escape` each remove it immediately, and at most three are on screen
with the newest first.

**SPA-33** — the sitting's decisions collect in the side history.
Strength: MUST · Layer: chrome · Source: preserves `WEB-18`, `WEB-46`.
Every receipt of this sitting collects in the side history, newest first, each carrying the
outcome it recorded, and each still undoes. On the phone, where there is no room for the column,
the history is behind its own control with a count on it.

**SPA-34** — two announcement channels, each saying only its own thing.
Strength: MUST · Layer: chrome · Source: preserves `WEB-52`, `WEB-59`.
The page carries exactly one `role=status` connection line, whose text is only ever a connection
state, and exactly one separate `role=log` region that carries notices and receipts. Neither
claims the other's role, and no third live region exists.

### The read pages

**SPA-35** — a row is a title and one sentence, with one pill.
Strength: MUST · Layer: chrome · Source: preserves `WEB-40`, `WEB-41`.
On every list page, a row's first element is its title — a link exactly when the row opens a page
— followed by exactly one meta sentence carrying no separator chain; a status renders as the one
pill style and no second badge class exists; a priority renders as plain mono text, `P0` in the
reject hue.

**SPA-36** — tables are borderless but for the row hairline.
Strength: MUST · Layer: chrome · Source: preserves `WEB-42`, `WEB-43`.
On every table page, each `th`/`td` draws either no border or exactly the one-pixel row separator
in the surface token, and every numeric cell is right-aligned in the mono stack.

**SPA-37** — nothing overflows sideways, on any route, at any of the three widths.
Strength: MUST · Layer: chrome · Source: preserves `WEB-44`.
At 390, 820 and 1280 px wide, every in-scope route satisfies
`documentElement.scrollWidth <= documentElement.clientWidth`, each page renders its own heading,
each preview renders its own card and no shell, and the swept list of routes is checked against
the route registry so a new arm cannot be added and left unmeasured on a phone.

**SPA-38** — the drawer is the navigation, and it marks where you are.
Strength: MUST · Layer: chrome · Source: preserves `WEB-36`, `WEB-37`, `WEB-39`.
Opening the menu shows the drawer on the desk surface with the search field ahead of every
destination in document order, each destination a row separated by the one hairline and at least
44 CSS px tall; on each destination exactly one drawer link is marked current, by a left rule
rather than a fill, and no other link carries one.

**SPA-39** — search answers from the drawer and reaches a cited record.
Strength: MUST · Layer: chrome · Source: preserves `WEB-39`; ADR-016's served search.
A query entered in the drawer's search field lands on `/search`, the results name the records
they cite, and following one reaches that record's own page. Search reads through the same store
path the CLI's search uses and writes nothing.

**SPA-40** — a reference previews in place, nests, and opens in a new tab.
Strength: MUST · Layer: chrome · Source: preserves `WEB-44`; ADR-016's 2026-09-11 amendment
(previews as product behaviour).
Hovering a reference link opens its preview without navigating; a reference inside a preview
opens its own preview; `Escape` closes what hover opened; and a click on the link opens the
referenced page in a new tab rather than replacing the deck.

**SPA-41** — markdown renders and raw HTML stays inert.
Strength: MUST · Layer: chrome · Source: preserves `WEB-54`; ADR-016's 2026-09-11 amendment.
Agent-authored markdown renders as formatted text, and HTML inside agent-authored text is
displayed as text: no element authored by an agent enters the document, and no script authored by
an agent executes. This holds for every field the SPA renders — attention bodies, task titles,
rule text and consequences alike — and it is the client's obligation now that the client builds
the DOM.
*Quality constraints:* this is the injection boundary of the whole slice. A client that assigns
agent text to `innerHTML` fails it whatever the server sent.

**SPA-42** — the read journey works on the phone.
Strength: MUST · Layer: chrome · Source: preserves `WEB-44`, `WEB-40`.
At 390 px, navigating from the deck through the drawer to the list pages and into a detail page
reaches the seeded records, with the URL-encoded parameterised routes resolving and no sideways
overflow on the way.

### Decided, plans, subscriptions, sprints and deployments

**SPA-43** — the decisions room lists newest first and each row undoes.
Strength: MUST · Layer: chrome · Source: preserves `WEB-26`, `WEB-32`; `docs/PRD.md` (the
decision room).
`/decided` lists the recorded decisions newest first, and undoing one from that page reopens the
item on the board.

**SPA-44** — opening a draft plan moves the real task.
Strength: MUST · Layer: chrome · Source: preserves `WEB-55` (the `plan/../open` verb).
Opening a draft plan from `/plans` posts `plan/{project}/{id}/open` and the real task moves to
`todo` on the board — the write is the ledger's, observed through the CLI, not a client-side
state change.

**SPA-45** — subscription pause and resume each persist.
Strength: MUST · Layer: chrome · Source: preserves `WEB-55` (the
`subscription/../pause|resume` verbs).
Pausing a subscription from `/subscriptions` persists the paused state with its actor, and
resuming persists the resumed state; each is durable across a reload and visible to the CLI.

**SPA-46** — a dead-lettered subscription names its code.
Strength: MUST · Layer: chrome · Source: preserves `WEB-40`.
A dead-lettered subscription row shows the stable error code the dispatcher recorded, verbatim,
rather than a generic failure sentence.

**SPA-47** — sprints and deployments keep their index and their detail.
Strength: MUST · Layer: chrome · Source: preserves `WEB-44`; ADR-039, ADR-043 (the deployment
ledger's served pages).
`/sprints`, `/sprints/{project}` and `/sprint/{project}/{id}` render the empty, current, history
and detail states, and `/deployments` and `/deployment/{project}/{id}` render the cross-board
matrix and the attempt detail, each from the projection and each reachable by navigation.

### The live channel

**SPA-48** — `/live` stays a notice channel.
Strength: MUST · Layer: chrome · Source: ADR-048 §5 ("`/live` is unchanged in kind");
`rust/serve.rs:1096`, `rust/serve.rs:1121`, `rust/serve.rs:1131`, `rust/serve.rs:1142`.
The frames `/live` carries stay exactly the four kinds the server sends today: `ready` with the
revision fingerprint, a notice from the scan, `refresh` with the new fingerprint, and
`heartbeat`. A change made through the CLI reaches the open page as a notice and the page
converges on it without a reload.

**SPA-49** — `/live` carries no body, no credential and no capability.
Strength: MUST · Layer: chrome · Source: ADR-048 §5; ADR-016's frame-content rule.
No frame carries a task or attention body, a credential, a cookie, a lease token or a write
capability, and no frame is a write. Authorized summaries are permitted where the client needs
them; replicated state is not — the socket tells the page that something changed and what kind of
thing it was, and the page reads the projection for the rest.
*Failure behaviour:* a client that cannot reach the projection shows a disconnected state and
holds the operator's in-flight work; it does not render from the socket alone.

### Payload

**SPA-50** — the landing payload gets smaller, and the number that exists is an observation.
Strength: MUST · Layer: http · Source: ADR-048 §3 ("the **1.32 MB landing payload is a real
defect**"); ADR-047 §9.
The landing document plus the bundle it loads is smaller than the current landing page. The
measurement that makes this observable is ADR-048 §3's: `/` answered in **55 ms at 1.32 MB** on
`hax` on 2026-09-17, beside `/attention` at 0.35 ms / 46 KB and `/boards` at 54 ms / 53 KB.
**That figure is a baseline, not a budget**: this specification sets no byte target, no latency
target and no availability target, and none may be inferred from the numbers above. The target
this requirement will eventually be measured against is §7 OQ-5, owned by `t-bf255880`; until it
is answered, the observable obligation is the strict inequality against the measured baseline.

### What is retired

**SPA-51** — with no script there is no page, and that is the decision.
Strength: MUST · Layer: chrome · Source: ADR-048 §6 (the first accepted cost); ADR-047 §3
(supersession).
With script execution disabled in the browser, an in-scope route renders its shell and nothing
data-bearing: no `article.item`, no `form.decide`, no list of rows, no table of cells, and no
element carrying `data-testid="app-root"` anywhere in the document. A build that still serves a
working scriptless list fails this requirement, because it can only be doing so from a second
renderer this slice does not keep.
*What this supersedes.* `docs/specs/web-ui.md` WEB-56 ("with scripting off, `/` is a plain list
that still works") and WEB-58 ("the deck is a progressive enhancement in the stylesheet") are
**superseded by this requirement** for the surfaces the SPA serves, and the tests that pin the
scriptless list are deleted with the markup they measure rather than left green against a
surface that no longer exists. `docs/specs/web-ui.md` is **not edited by this slice** — the
supersession is recorded here, by reference, as ADR-048 §6 requires.
*What is deliberately kept:* the ADR-042 §5 card order — question, context, recommended,
alternatives, custom answer, folded body, meta — was the reason the scriptless page read
correctly, and it remains the order the mounted card renders in. Losing the fallback does not
license reordering the card.
*What follows and is not decided here:* WEB-38 ("navigation works with no script at all", an
`http` GET of each destination returning that page's heading) falls in the same loss once the
server-rendered pages stop answering with their own headings. Whether that happens at the
cutover or at the end of it is §7 OQ-2's question, not this requirement's.

### The rendered result keeps ADR-046

These requirements preserve `docs/specs/web-ui.md`'s design-system obligations that are about
**the rendered result** rather than about the server-rendering mechanism. The wording that
*decides* a hue, a radius or a ratio stays in `web-ui.md` and in ADR-046 and is not re-decided
here; the figures §6 rests a conformance sentence on are reproduced in SPA-56 so that sentence
rests on numbers rather than on a pointer.

**SPA-52** — the deck's surfaces, fills and one motion survive the rebuild.
Strength: MUST · Layer: chrome · Source: preserves `WEB-12`, `WEB-13`, `WEB-14`, `WEB-15`,
`WEB-16`, `WEB-20`; ADR-046.
In the mounted page: the recommended answer leads on its outcome fill with the alternatives on
the neutral surface; no element is boxed beyond the hairline allowlist; the card advance is the
one motion, 140 ms each way, running once; a projection refresh never re-animates an unchanged
current card; and `prefers-reduced-motion: reduce` removes the advance while the queue still
advances.

**SPA-53** — the question is still the hero.
Strength: MUST · Layer: chrome · Source: preserves `WEB-01`, `WEB-02`, `WEB-07`; ADR-046.
The current card's question is set in the serif stack at the weight and the single clamped size
`web-ui.md` WEB-02 decides, no other element on screen is larger, and the line-count bound holds
at each of the three widths.

**SPA-54** — focus is visible and a thumb can hit every deck control.
Strength: MUST · Layer: chrome · Source: preserves `WEB-45`, `WEB-51`; WCAG 2.2 SC 2.4.7, 2.4.11;
Apple HIG's 44 pt minimum.
Every focusable element in the mounted page shows the focus ring while focused, and at 390 px
every deck control has a bounding box of at least 44 × 44 CSS px. The 44 is the product
commitment `web-ui.md` §6 records, above AA's own 24 × 24, and is not a conformance claim.

**SPA-55** — the card is named by its question.
Strength: MUST · Layer: chrome · Source: preserves `WEB-53`; WCAG 2.2 SC 4.1.2.
The current card's accessible name, read from the accessibility tree, equals its question text,
and every input and textarea in the mounted page has a programmatic label.

**SPA-56** — the design-system invariants hold over the bundle's own stylesheet bytes.
Strength: MUST · Layer: unit · Source: ADR-048 §6 ("WEB's real-Chrome contracts are **re-proven
on the SPA**, not assumed to carry over"); preserves `WEB-03`, `WEB-04`, `WEB-05`, `WEB-06`,
`WEB-08`, `WEB-09`, `WEB-10`, `WEB-11`, `WEB-21`, `WEB-22`, `WEB-23`, `WEB-24`, `WEB-27`,
`WEB-28`, `WEB-29`, `WEB-39`, `WEB-41`, `WEB-42`, `WEB-47`, `WEB-48`, `WEB-49`, `WEB-50`,
`WEB-52` — the twenty-three WEB obligations whose evidence layer is `unit`.
This requirement is the `unit` half only, and deliberately carries no clause about Chrome: the
`chrome`-layer WEB obligations in the same range are already preserved with named evidence by
`SPA-14`, `SPA-15`, `SPA-16`, `SPA-17`, `SPA-18`, `SPA-21`, `SPA-26`, `SPA-27`, `SPA-28`,
`SPA-29`, `SPA-30`, `SPA-33`, `SPA-36`, `SPA-37`, `SPA-38`, `SPA-40`, `SPA-42`, `SPA-43`,
`SPA-47`, `SPA-52`, `SPA-53` and `SPA-54`, so re-adding a browser clause here would duplicate
them rather than cover anything.
Read over the stylesheet the bundle actually ships, and over the strings the application
renders, all of the following hold: the token block declares exactly the hexes listed below and
no other colour token, and no hex literal appears anywhere outside it (WEB-08); every
foreground/background pair the stylesheet uses clears 4.5:1 and every fill and focus pair clears
3:1, at the ratios listed below (WEB-48, WEB-50); `--overlay` is never placed on `--surface0`
(WEB-49); exactly one pill rule exists and no second badge class is rendered (WEB-41); every
`border-radius` is `8px` or `999px` and every border and outline belongs to the hairline and
focus-ring allowlist (WEB-09, WEB-10); exactly one quiet keys line is rendered, with the digits
on the answer buttons and no `<kbd>` element (WEB-27); no rendered meta string is a dot chain
(WEB-22, WEB-23) and the copy strings WEB-24, WEB-28 and WEB-29 pin are exact; an outcome hue
appears only on a selector carrying an outcome or a status, with exactly one coloured answer
fill and it is the recommended choice (WEB-11); `var(--mono)` is named only on the selectors
WEB-05 allows — ids, keys,
code, priorities, the keys line and numeric cells; prose blocks stay bounded to `70ch` (WEB-06);
the type scale is declared with no `text-transform` and no `letter-spacing` (WEB-04); no
heading or link text carries a glyph prefix (WEB-03); the deck's body region declares the one
bottom fade (WEB-21); the search input precedes every destination in the drawer markup
(WEB-39); tables declare only the row hairline (WEB-42); every answer is one grid row and the
answer panel declares no scroll of its own (WEB-47); and every input and textarea is
programmatically labelled with exactly one `role=status` and one separate `role=log` region
(WEB-52).
*The numbers this carries, copied from `docs/specs/web-ui.md` so §6 rests on them rather than on
a citation.* The token block (WEB-08) is `--base:#1e1e2e`, `--mantle:#181825`,
`--surface0:#313244`, `--surface1:#45475a`, `--text:#cdd6f4`, `--subtext:#a6adc8`,
`--overlay:#9399b2`, `--green:#a6e3a1`, `--red:#f38ba8`, `--yellow:#f9e2af`, `--blue:#89b4fa`,
`--link:#89b4fa`, `--focus:#b4befe`, and no other colour token. The measured WCAG contrast
ratios every used pair must still clear at ≥ 4.5:1 (WEB-48) are: `--text` on `--base` 11.34, on
`--mantle` 12.14, on `--surface0` 8.69; `--subtext` on `--base` 7.37, on `--mantle` 7.89, on
`--surface0` 5.65; `--overlay` on `--base` 5.81, on `--mantle` 6.22; `--base` on `--green`
11.03, on `--red` 7.08, on `--yellow` 12.91, on `--blue` 7.79; `--green` on `--surface0` 8.46,
`--red` 5.43, `--yellow` 9.89, `--blue` 5.97; `--link` on `--base` 7.79, on `--mantle` 8.34.
`--overlay` is never placed on `--surface0`, where it measures 4.45:1 and fails AA for text
under 24px (WEB-49). The non-text ratios (WEB-50) are `--focus` on `--base` 9.17 and on
`--mantle` 9.81, both ≥ 3:1, and the recommended answer's fill against the page at 11.03
(`--green`), 7.08 (`--red`), 12.91 (`--yellow`) and 7.79 (`--blue`). `--overlay` is
Catppuccin Mocha `overlay2` `#9399b2` and not the design plan's `#6c7086`, which measures
3.36:1 on `--base` against the 4.6:1 the plan claimed for it; `overlay1` `#7f849c` reaches only
4.44:1, so `overlay2` is the lightest step that clears the rule (`web-ui.md` §7 OQ-1, resolved
2026-09-17). A token change that drops any pair above below its threshold fails this
requirement, and the test computes the ratios from the tokens rather than pinning the printed
numbers.

**SPA-57** — the document still reaches no third party.
Strength: MUST · Layer: unit · Source: preserves `WEB-54`; ADR-048 §1.
Every `href` and `src` in the served document and in the bundle is same-document or
site-absolute: no webfont, no image host, no CDN, no external `script src`, and no runtime
fetch to any origin but this one. The bundle being built elsewhere does not put anything on the
page from elsewhere.
*Quality constraints:* this is what keeps the edge's existing `self`-scoped policy applicable
without kanban sending a policy header of its own.

## 4. Acceptance examples

Given/When/Then in plain prose. Each heading names the requirement IDs it proves.

### A1 — the artefact (SPA-01, SPA-02, SPA-03)

*Given* a release built on the build host and installed on `hig`, with the build tree and the
source tree absent from that host and no Node-shaped runtime installed on it,
*when* the unit is activated and `kanban serve` answers `/`,
*then* the page mounts from the executable's own bytes alone; no process on the host is a Node,
Bun, npm, pnpm, Yarn or Corepack process; and the deploy receipt names both the executable that
resolves inside the activated `releases/<id>` and the fingerprint of the bundle that executable
serves.

### A2 — readiness (SPA-04, SPA-05)

*Given* a seeded board and a freshly loaded `/` in real Chrome,
*when* the harness takes its first look at the instant navigation returns and then waits on the
mounted root,
*then* the first look finds no data-bearing element, and `wait_for_app_root(tab,
ui::APP_ROOT)` returns a visible element whose `data-testid` is `app-root`, after which the
deck's own elements are present.

### A3 — the projection, read (SPA-06, SPA-07, SPA-08, SPA-09, SPA-13)

*Given* two registered boards, one of which the principal may not read, and one task currently
leased with a live lease token,
*when* each page's JSON route is read over HTTP and the boards are hashed before and after,
*then* every page's data came from a `GET`, the unreadable board appears in no body, no body
contains a lease token value or the string `leaseToken`, the board files are byte-identical
afterwards, and every handler that produced a body did so by calling a `Store` method the CLI
also calls — no browser-facing handler holds SQL of its own.

### A4 — an unauthorized cross-origin write (SPA-10, SPA-11, SPA-12)

*Given* `kanban serve` running with `--actor-header X-Auth-Request-Email` and one open attention
item,
*when* a `POST` to `attention/{project}/{id}/reply` arrives carrying an `Origin` of another site,
with a JSON body naming `"actor": "somebody-else"`,
*then* the response is `403` with the sentence `The action did not come from this site.`, the
item is still `open` with no `decision`, no event was appended, and the supplied actor appears
nowhere; *and when* the same write arrives same-origin with a valid edge header, *then* it is
recorded with the header's validated value as the actor, and a fifth verb `POST`ed to any other
path is `404` before any of this is reached.

### A5 — the deck, happy path (SPA-14, SPA-20, SPA-26, SPA-27, SPA-28, SPA-33, SPA-43)

*Given* three open carded attention items on a board,
*when* George clicks the recommended answer on the first card,
*then* the decision is recorded on the board, the pressed control read `Sending…` on its own
fill while the POST was open, a receipt carrying that outcome lands in the side history, and the
second card is current; *when* he skips the second, *then* the third is current and the skipped
row is still `open` with no `decision`; *when* he answers the third in his own words with both
halves, *then* the recorded decision carries the verdict he picked and the reply he wrote; *when*
he undoes his first decision, *then* that item is reopened on the board and is current again;
*and when* the deck finally empties, *then* the empty-deck copy and its link to `/decided` are on
screen and `/decided` lists the sitting's decisions newest first.

### A6 — a board refusal (SPA-21, SPA-22)

*Given* a card on screen and, unknown to the page, the same item already resolved by another
lane,
*when* George types `hold it` into the note and clicks an answer,
*then* the store refuses with `attention {id} was already resolved by {who} — it is history, not
a queue entry`, that sentence is on the card verbatim, the card is the card on screen again at
its own position, every answer control is enabled, the deck's count is unchanged, no receipt
appeared, and `hold it` is still in the note field.

### A7 — the concurrent rewrite: the choices changed underneath (SPA-22, SPA-21)

*Given* a card rendered with choices `a`, `b` and `c`, and the item's authored choices rewritten
through the CLI to `x` and `y` while that card is on screen,
*when* George answers `b`,
*then* the store refuses with `attention {id} has no choice b; its choices are x, y`, that
sentence is shown verbatim, nothing is recorded, and after the refreshed card arrives the answers
on screen are `x` and `y` — answering `x` then records normally.

### A8 — an in-flight answer meets a live notice (SPA-16, SPA-17, SPA-18, SPA-25, SPA-30)

*Given* the deck with the reader inside a long card — its own column scrolled to the end, the
long-form region part-way down — a reply half typed and a verdict picked,
*when* another lane raises a question through the CLI and the `/live` socket brings the
projection, and then the same notice is delivered a second time,
*then* the typed text is still in its field, the verdict is still picked, both scroll positions
are unchanged, the note field is still inside the viewport, the page acted and rendered once for
the two deliveries, and submitting afterwards records exactly the verdict that was picked.

### A9 — the incomplete free-text answer (SPA-19, SPA-24)

*Given* the own-words fold open with a reply typed and no verdict picked,
*when* George submits,
*then* no POST is made, the one fixed sentence appears in the composer's own refusal channel,
focus is on the verdict group, and the board records nothing; *when* he picks a verdict and
presses `Escape`, *then* the verdict is released and the reply is untouched; *when* he picks one
again and submits, *then* the answer records without a reload.

### A10 — the read pages and the drawer (SPA-35, SPA-36, SPA-37, SPA-38, SPA-39, SPA-42)

*Given* a board with one task in each status, one of them `P0`, a sprint, a deployment and a
subscription,
*when* every in-scope route is loaded at 390, 820 and 1280 px and the drawer is opened on each,
*then* no route overflows sideways, every list row is a title followed by one meta sentence with
one pill and a mono priority, every table cell draws only the row hairline with numerics right
aligned in mono, the drawer shows search ahead of every destination with the current one marked
by a rule, and a search from the drawer reaches a cited record's own page.

### A11 — previews and agent-authored text (SPA-40, SPA-41)

*Given* an attention body containing markdown, a raw `<script>` tag and a reference to a task,
*when* the card is read in Chrome and the reference is hovered and then clicked,
*then* the markdown is formatted, the script tag is visible as text and never executed, the hover
opens a preview that itself previews a nested reference, `Escape` closes them, and the click opens
the task page in a new tab.

### A12 — the ledger writes that are not decisions (SPA-44, SPA-45, SPA-46, SPA-47)

*Given* a drafted plan, an active subscription, a dead-lettered subscription, a sprint and a
verified deployment,
*when* the plan is opened and the subscription paused and then resumed from the browser,
*then* the CLI reports the plan's task in `todo` and the subscription's paused and resumed states
in turn with their actor; the dead-lettered row names its stable error code verbatim; and the
sprint and deployment index and detail pages render their own content.

### A13 — the live channel (SPA-48, SPA-49, SPA-34, SPA-15, SPA-25, SPA-32)

*Given* an open page with the socket connected,
*when* another lane makes a change through the CLI, then floods a burst of changes, then the
socket drops and reconnects,
*then* the change arrives as a notice in the `role=log` region without a reload, the burst
surfaces one summary rather than one notice per change, the reconnect replays no history, the
connection line's text is only ever a connection state, no frame carried a body or a token, the
toast is readable for its full window and dismissible by click and by `Escape`, and the sitting's
receipts are still on screen with their undo reachable.

### A14 — the payload and what was lost (SPA-50, SPA-51)

*Given* the SPA build serving the seeded landing route,
*when* `/` is fetched and its bundle with it, and then the same route is loaded in Chrome with
script execution disabled,
*then* the landing document plus its bundle is strictly smaller than the 1.32 MB ADR-048 §3
measured for the current `/` — recorded as a comparison against that baseline, with no byte
target asserted — and the scriptless load renders only the shell, which is the retirement of
WEB-56 and WEB-58 this specification records rather than a defect.

### Categories deliberately not exercised here, and why

- **Authentication and session handling.** Not applicable: kanban implements none and there is no
  flag to change that (ADR-016). The edge is the Google SSO for `*.geoy.ws`. A11y of the login
  screen, session fixation and token lifetime are the edge's and are not restated here.
- **Multi-user concurrency and tenancy isolation.** Not applicable: there is one operator. The
  concurrency that does exist — agent lanes writing to the ledger while the operator reads it —
  is exercised by A7 and A8, which are the two ways it reaches the browser.
- **Migration and rollback of stored data.** Not applicable: this slice stores nothing and
  migrates nothing (§5).
- **Load, capacity and failover.** Deliberately unexercised: ADR-048 §3 says plainly that this
  change does not reduce measured server CPU and was not chosen for that, and ADR-047 §9 forbids
  inventing a commitment here. §7 OQ-5 carries the one number that is in scope.

## 5. Contracts and data

- **Interface version or schema:** three interfaces. (1) The HTML document and the embedded
  bundle it loads — one document, one application root (SPA-04). (2) The read-only JSON
  projection — one `GET` route per page arm of `render` (SPA-06); its **naming and versioning
  are decided** by `t-19d4c16d` (§7 OQ-3, resolved 2026-09-19) in
  `docs/api/kanban-web.openapi.yaml`, which this specification points at rather than restates.
  (3) The four form POSTs, unchanged in path, method and field names, and the `/live`
  WebSocket, unchanged in frame kinds (SPA-48). The OpenAPI document that describes (2) and
  (3) is `docs/api/kanban-web.openapi.yaml`, introduced by `t-19d4c16d` and read alongside
  `docs/api/README.md`; it is a contract for the projection `t-88814b7a` builds, not a record
  of a shipped surface.
- **Data invariants:** the ledger is the only state. The browser holds no durable state of its
  own beyond the current session's view; a reload reconstructs everything from the projection.
  No JSON body and no frame ever carries a lease token, a credential or a write capability
  (SPA-09, SPA-49). The ADR-042 §5 card order is preserved in the rendered card (SPA-51). The
  decision record and its resolution stay derived by the store's composer, never by the client
  (SPA-20).
- **Migration:** none. No board schema change, no column, no event kind, no stored-shape change
  in either direction. The change is entirely in how the same data reaches the same reader.
- **Compatibility:** the CLI and MCP surfaces are untouched and see the same boards. A browser
  with scripting disabled loses the page (SPA-51) — that is the one compatibility loss, accepted
  by ADR-048 §6 and recorded rather than mitigated. Whether the server-rendered pages stay
  reachable during the cutover is §7 OQ-2, owned by `t-1f495a7f`.
- **Ownership:** the boards are the ledger's, read and written through `Store`. The projection
  owns no data; the bundle owns no data; the receipt's bundle fingerprint is owned by the release
  process (ADR-044 §2).

## 6. Quality and security

- **Reliability:** the socket is a notification channel, not a source of truth: a page that loses
  it holds the operator's in-flight work and converges from the projection when it returns
  (SPA-25, SPA-49). No availability or uptime commitment is made or implied.
- **Accessibility:** WCAG 2.2 level AA is the target for the surfaces this slice serves,
  asserted by SPA-34 (two announcement channels), SPA-54 (visible focus), SPA-55 (accessible
  names and programmatic labels) and SPA-56, which carries the arithmetic the claim rests on:
  every used text pair measures at least 4.5:1 — the worst of them is `--subtext` on
  `--surface0` at 5.65:1, and `--overlay` on `--surface0` at 4.45:1 is excluded for that reason
  — while the focus ring measures 9.17:1 on `--base` and 9.81:1 on `--mantle` and the
  recommended answer's fill 7.08:1 to 12.91:1 against the page, all above the 3:1 SC 1.4.11
  asks of non-text. Conformance is claimed only for what those requirements measure, not as a
  full-page audit. The separate 44 × 44 CSS px hit target of SPA-54 is **not** the standard's
  figure: AA's own is 24 × 24 (SC 2.5.8), and 44 is Apple HIG's, recorded by
  `docs/specs/web-ui.md` §6 as a product commitment George may lower to 24 without touching AA
  conformance.
- **Privacy:** the document reaches no third party (SPA-57), so no page view leaves the estate.
  Board content is served only to the principal the store authorizes (SPA-08).
- **Security:** the write surface does not grow (SPA-10), same-origin gating is unchanged
  (SPA-11), the actor is the trusted edge's (SPA-12), no lease token is ever serialised (SPA-09,
  SPA-49), and agent-authored text is inert in a client that now builds the DOM itself (SPA-41 —
  the injection boundary of the slice). Authentication, session management and access-control
  policy are out of scope with reason: kanban implements none of them and the edge owns them.
  Individual OWASP ASVS requirement IDs are not claimed; the applicability here is by chapter
  theme, as `docs/specs/web-ui.md` §6 states for the same surface.
- **Operability:** no hot reload; a new bundle arrives by restart, as a new binary does
  (ADR-016). The deploy receipt proves both halves of what is served (SPA-03), so an operator can
  answer "which UI is live" from the receipt rather than from a browser.
- **Performance:** one observation, explicitly **not a budget**. ADR-048 §3 measured, on `hax` on
  2026-09-17: `kanban-serve` at 0.0% CPU and 7.2 CPU-seconds over 7054 s of uptime, RSS 79 MB;
  `/attention` 0.35 ms at 46 KB; `/boards` 54 ms at 53 KB; `/` 55 ms at 1.32 MB. The 54 ms is
  SQLite reads across 24 boards and is paid identically whether the answer is HTML or JSON, so
  this change does not reduce it and was not chosen to. The 1.32 MB is the one number ADR-048
  calls a defect; SPA-50 turns it into a strict inequality against that baseline and nothing
  more. Any byte, latency or availability target is §7 OQ-5's to answer, not this document's to
  guess (ADR-047 §9).

## 7. Open questions

| ID | Question | Owner | Status | Gate it blocks |
| --- | --- | --- | --- | --- |
| OQ-1 | Which bundler and which React version? ADR-048's Consequences leave both open and name this specification as the place they are decided; the constraint they must satisfy is SPA-01 and SPA-02 — build-time only, output embedded, nothing of that shape on `hax` or `hig`. | `t-992e40aa` | **resolved 2026-09-19** — React 19.2.0 and TypeScript 5.9.3 `strict`, bundled by esbuild 0.25.12 run under Bun; lint is `tsc --noEmit` plus Biome 2.3.14. esbuild rather than Bun's own bundler because `web/dist` is committed and the bundler's version must be pinned by the lockfile, which Bun's own is not. The built bundle is committed under `web/dist`, embedded by `include_bytes!` through a generated table (`build.rs`), and proved byte-for-byte reproducible by `web/check-reproducible.sh`; the release path stays cargo-only and offline. Recorded in `web/README.md` and ADR-048's 2026-09-19 addendum. | the embedded bundle and its build step |
| OQ-2 | Do the server-rendered pages stay reachable while the SPA is cut over, or does the cutover replace them route by route? This decides whether `WEB-38`'s scriptless destination GET survives the first wave and whether two renderers are live at once. | `t-1f495a7f` | open | the Needs-you cutover |
| OQ-3 | What are the JSON routes called, and how are they versioned? Deliberately not invented here: SPA-06 pins that one read-only `GET` route exists per page arm and what it may do, not its name. | `t-19d4c16d` | **resolved 2026-09-19** — `docs/api/kanban-web.openapi.yaml` (OpenAPI 3.0.3) with `docs/api/README.md`. JSON under `/api/v1/…`, one `GET`-only route per page arm named after the arm, `application/json; charset=utf-8`; the path version is a compatibility boundary (additive fields stay `v1`, a removed or renamed field is `/api/v2`); the four POSTs keep their current paths unchanged (SPA-10). | the API contract |
| OQ-4 | How does the bundle fingerprint enter the deploy receipt — which ADR-044 §2 provenance field carries it, and is it verified against the running process or only against the package? | `t-992e40aa` | **resolved 2026-09-19** — the field is `bundleSha256`, written beside `manifestSha256` in both the package manifest and the `formatVersion` 2 receipt, and additive: a receipt without it stays valid, and `receipt_provenance_ok` only shape-checks it when present. It is measured by running the packaged executable (`kanban version`, second line `bundle <sha256>`), and it is verified against a RUNNING PROCESS, not only the package: `install` runs the INSTALLED executable and refuses the activation before the receipt is written if the two disagree (`prove_installed_bundle` in `scripts/hig-release.sh`, beside `serve_prove_release`'s `/proc/<MainPID>/exe` proof). | the release receipt (ADR-044 §2), and SPA-03's process evidence |
| OQ-5 | Do `/search` and `/preview` move in the first wave, and what is the actual landing-payload target the 1.32 MB baseline is being reduced *to*? Both are scope calls on the last row of the epic. | `t-bf255880` / George | open | the remaining pages and the payload fix |

## 8. Verification

The planned evidence for every mandatory requirement. This table is the draft of the matrix rows;
`docs/testing/compiled-rust-e2e-matrix.md` is the trace of record and carries the same rows
verbatim. `Layer` is named precisely and is never `e2e` for an in-process test.

`Test name` is the **existing** test that observes the behaviour today, on the server-rendered
surface, enumerated against this build with
`cargo test --locked --test e2e -- --list` (chrome and http rows) and by function definition in
`rust/serve.rs`'s `mod tests` (unit rows; the lib test target does not compile in this
documentation worktree, so the unit names are verified as `fn <name>(` in the source). The
implementation rows re-point those tests at the SPA — that is what ADR-048 §6's "re-proven on the
SPA" means in practice. `none` means no test observes the behaviour today, and the Note names the
row that must write one.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `SPA-01` | MUST | process | `hig_release_script_install_restarts_kanban_serve_and_proves_the_served_exe` | the served executable; the bundle half landed 2026-09-19 with `t-992e40aa` — `an_asset_answers_only_under_its_own_content_hashed_name_unit` (every asset answers from the embedded table under the name its bytes hash to, a wrong hash is 404, and nothing is read from the filesystem) and `the_app_shell_mounts_the_react_root_by_test_id_in_real_chrome` (the mounted page loaded the two embedded assets and nothing from any other origin) |
| `SPA-02` | MUST | process | `none` | no automated coverage. The build-host half is proved by a gate command rather than a test: `PATH=/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.cargo/bin cargo build --locked --release` succeeds with no Node, Bun or npm on `PATH`, because `web/dist` is committed and `build.rs` only reads it (run 2026-09-19 by `t-992e40aa`). The package half is held by `validate_release_files`'s exact-entries rule — a package may contain `manifest.json` and the declared binaries and nothing else. The serving-host half is an inventory of `hax` and `hig` that no compiled test can take, and stays owed by the epic |
| `SPA-03` | MUST | process | `compiled_binary_refuses_unknown_flags_instead_of_writing_to_the_wrong_board` | OQ-4 resolved 2026-09-19: the field is `bundleSha256`, verified against the running process. That case asserts the `bundle <sha256>` second line of `kanban version` against the bundle committed in this worktree; `the_version_banner_names_the_embedded_bundle_unit` recomputes the fingerprint from the embedded bytes; `hig_release_script_provenance_fields_leave_the_manifest_bytes_and_release_id_unchanged` pins `bundleSha256` in the manifest; and `hig_release_script_installs_two_distinct_builds_of_one_commit_as_two_releases` installs a rebuild carrying a different bundle through `prove_installed_bundle`, which runs the INSTALLED executable before the activation receipt is written |
| `SPA-04` | MUST | chrome | `the_app_shell_mounts_the_react_root_by_test_id_in_real_chrome` | the product rather than the fixture, landed 2026-09-19: `/app` on a spawned `kanban serve`, `wait_for_app_root(&tab, ui::APP_ROOT)` returns the mounted `<main>`, its heading reads `Needs you` and its live line reads `live`. `an_async_mounted_root_is_found_by_test_id_in_real_chrome` keeps the harness half. The id is stable across routes only in the sense the first wave can prove — one route mounts today; the cutover (`t-1f495a7f`) is what puts a second one under the same root |
| `SPA-05` | MUST | chrome | `the_app_shell_mounts_the_react_root_by_test_id_in_real_chrome` | the same case proves the split over the SERVED BYTES — the first response carries `data-testid=app-root-shell` and no `app-root` — because the product's mount is a real bundle over loopback, and asserting that a naive look loses the race to it would be a stopwatch rather than a contract; the naive look is still taken and reported. `the_app_shell_closes_its_head_exactly_once_unit` holds the shell's shape. `wait_for_shell_ready` (`tests/e2e.rs`) still serves the server-rendered pages and becomes `wait_for_app_root(tab, ui::APP_ROOT)` at the cutover (`t-1f495a7f`) |
| `SPA-06` | MUST | http | `the_json_needs_you_projection_answers_the_same_cards_as_the_cli_over_http` | landed 2026-09-19 with `t-88814b7a`, over the compiled binary on a real socket: the first wave of five routes — `/api/v1/needs-you`, `/api/v1/boards`, `/api/v1/board/{project}`, `/api/v1/lanes`, `/api/v1/task/{project}/{id}` — answers `application/json; charset=utf-8` from `rust/projection.rs`, and the named case asserts one board's queue row for row against `kb att list --status open --json` on that board (the single-board order rule; the cross-board key is the board name) rather than against itself, because a second read model that agreed on the day is the drift ADR-048 §5 forbids. `a_capped_json_listing_says_it_was_capped_over_http` holds the `ListEnvelope` fields. contract: `docs/api/kanban-web.openapi.yaml#/paths` — fifteen `GET` operations, one per page arm, each carrying `x-requirement: [SPA-06, …]`; the ten not yet built answer the same non-enumerating `404` as an unknown board Extended 2026-09-19 by `t-4b9501b3` with two INTEGRATION cases in `tests/authz_bypass_matrix_e2e.rs`, both over the compiled binary on a real socket with no browser: `the_json_routes_answer_the_same_rows_as_the_cli_over_http` is the projection invariant — `/api/v1/board/{project}`, `/api/v1/task/{project}/{id}`, `/api/v1/lanes` and `/api/v1/needs-you` compared against `task list --json`, `task show --json`, `sitrep list --lane L --limit 200 --json` and `attention list --status open --json` run as the same process identity against the same estate, field by field on the keys the two shapes share, positionally and never sorted first, with an anchor set asserted present on both sides so a rename cannot silently empty the comparison — and `every_json_listing_says_whether_it_was_capped_over_http` crosses all three real bounds (1001 open rows against the 1000-row queue, 210 sitreps across 21 lanes against the 200-row per-board scan, because posting archives all but a lane's last ten, and 51 notes against `DETAIL_ROWS`) and asserts `limit: null` with `truncated: false` on the two listings whose store call takes no bound. |
| `SPA-07` | MUST | unit | `the_projection_reaches_nothing_but_the_store_and_the_registry` | landed 2026-09-19 with `t-88814b7a`, as a source-reading unit test of the same shape as `no_page_can_reach_a_method_that_writes` and sharing its `STORE_MUTATORS` list: `rust/projection.rs` names no `&mut self` store method, no `rusqlite`, no `SELECT`, no connection and no statement, and imports nothing else from this crate than the model, the store, the registry and the page router's own board enumeration, which it shares rather than copies. contract: `docs/api/kanban-web.openapi.yaml` — every operation's `x-store-method` names the `Store` methods the arm it mirrors already calls, and the two `x-registry-method` exceptions are named rather than hidden; `getBoard`'s rules are read with `Registry::applicable_rule_summaries`, the registry's own `RuleSummary` projection of the `applicable_rules` the contract names, so the headline and byte count are derived once |
| `SPA-08` | MUST | http | `the_json_projection_refuses_unknown_retired_and_unauthorized_boards_with_one_body_over_http` | landed 2026-09-19 with `t-88814b7a`, beside `serve_hides_retired_boards_from_the_board_index_and_board_route`, which keeps the HTML arm's behaviour: an unknown board, a retired one, an ambiguous name and an absent row all answer `404` with the byte-identical body `{"error":"denied or not found"}`, the body never carries the board name or the retirement note, and the retired board is absent from `/api/v1/boards`. The page arms still answer `500` with the store's detailed text, which is the divergence recorded in `docs/api/README.md`. A board this principal may not read is not seeded: the estate is `direct`, where the guard permits everything (`rust/authz.rs:54`), so that case is held by the projection classifying the store's own `denied or not found` as this refusal rather than as a failure. contract: `docs/api/kanban-web.openapi.yaml#/components/responses/DeniedOrNotFound` — the store's own `denied or not found`, non-enumerating Extended 2026-09-19 by `t-4b9501b3` over a MANAGED estate, which this case could not seed: `the_five_json_routes_enforce_the_same_board_and_tag_authority_as_the_cli_over_http` (`tests/authz_bypass_matrix_e2e.rs`) binds the serving process a principal owning board A and holding nothing on board B, then asserts every named read of B, and of a row whose tag the caller lacks, answers `404` with the byte-identical body while the CLI refuses the same reads for the same identity; that the board listing hands over exactly the rows `task list` hands over, tagged row included, because the guard is the store's and the route adds no second rule; and that a grant added mid-run makes both surfaces answer an unchanged request. `no_json_refusal_names_a_board_row_or_tag_the_caller_may_not_see_over_http` searches every 4xx body for the invisible board's name, row title, row id, tag and lane. `the_lanes_route_withholds_the_sitreps_of_a_board_the_caller_may_not_read_over_http` (`t-c84850a1`, 2026-09-19) is live evidence here since the store gained the guard it was holding open: `Store::sitreps` now takes the same `self.authz.check_read(&[])` its sibling reads take — a sitrep carries no tags, so board scope is the whole subject — and `lane_groups` skips a board whose sitreps read is refused rather than refusing the whole enumeration, so `/api/v1/lanes` and the `/lanes` page answer the readable estate. The case binds a principal owning board A and holding nothing on board B, takes the CLI refusing `sitrep list` on B as its positive control, and asserts B's lane name and sitrep body are absent from the served JSON. One divergence is still recorded as an `#[ignore]`d case carrying the failing assertion rather than as a weakened one: `a_whole_estate_listing_serves_the_boards_the_caller_may_read_over_http` fails because `/api/v1/boards` and `/api/v1/needs-you` refuse entire instead of serving the readable subset (the CLI's `dashboard` refuses identically, so the two surfaces agree and it is `docs/api/README.md`'s non-enumeration rule that diverges). |
| `SPA-09` | MUST | http | `the_json_projection_never_serialises_a_lease_token_over_http` | landed 2026-09-19 with `t-88814b7a`: a live lease is taken, then `/api/v1/task/{project}/{id}` and `/api/v1/boards` are searched for the token's own bytes and for the key `leaseToken`, and the served claim's field set is asserted equal to `ClaimSummary`'s six — the token is structurally absent rather than filtered, because a filter can be forgotten and a type cannot. `compiled_binary_persists_across_processes_and_rotates_handoff_lease` is the same assertion on the CLI read model. contract: `docs/api/kanban-web.openapi.yaml#/components/schemas/ClaimSummary` — a task's holder is served as `ClaimSummary` (`rust/model.rs:559`), never as `Claim` (`rust/model.rs:501`, `lease_token` at `:508`), and `#/components/schemas/SubscriptionPosition` serves `leased` as a count with no delivery token `no_json_route_serialises_a_lease_token_over_http` (`t-4b9501b3`, 2026-09-19, `tests/authz_bypass_matrix_e2e.rs`) extends the search to all five routes in one case, on the raw response bytes, with the token taken from a real `kanban claim` and the holder still served beside it. |
| `SPA-10` | MUST | unit | `the_route_table_and_the_write_allowlist_are_unchanged_unit` | contract: `docs/api/kanban-web.openapi.yaml` — exactly four `POST` operations, at their current paths and outside `/api/v1`, and every JSON route `GET`-only `a_reply_naming_a_choice_the_row_no_longer_carries_is_refused_by_name_over_http` (`t-4b9501b3`, 2026-09-19, `tests/authz_bypass_matrix_e2e.rs`) drives ADR-042 §4 refusal 13 across the process boundary: the row's choices are rewritten between the render and the post, the stale key is refused `409` naming it, the row is still open afterwards, and the rewritten key is then accepted — so what was measured is the key and not a broken write path. |
| `SPA-11` | MUST | unit | `same_origin_post_is_csrf_defence_and_grants_no_capability` | the in-process proof over the handler; `serve_actor_header_uses_trusted_edge_value_and_refuses_bad_requests` keeps same-origin required across a compiled HTTP exchange. contract: `docs/api/kanban-web.openapi.yaml#/components/responses/Refused` — the product's own sentence, verbatim from `rust/serve.rs:497` `the_four_web_writes_are_refused_without_identity_and_across_origins_over_http` (`t-4b9501b3`, 2026-09-19, `tests/authz_bypass_matrix_e2e.rs`) puts all four POSTs through an absent `Origin` and a foreign one against the compiled binary — `403` carrying the product's sentence each time, before the actor is resolved and before the board is opened — and reads the board back to show nothing moved. Those refusals are `text/html`, while the contract declares `application/json` for `Refused`, `WriteRejected` and `WriteConflict`; that divergence is held by the `#[ignore]`d `the_write_refusals_answer_the_contracts_json_error_body_over_http` rather than by a weakened assertion. |
| `SPA-12` | MUST | http | `serve_actor_header_uses_trusted_edge_value_and_refuses_bad_requests` | `trusted_edge_resolution_stays_on_the_single_web_call_site` and `the_trusted_edge_actor_header_records_the_same_actor_over_a_socket` hold the other halves. contract: `docs/api/kanban-web.openapi.yaml#/components/securitySchemes/trustedEdge` — one scheme, `X-Auth-Request-Email`, overwritten by `proxy_set_header` so a client copy cannot be injected; fails closed with no identity; no bearer token exists in this estate `a_client_copy_of_the_actor_header_cannot_override_the_trusted_edge_value_over_http` (`t-4b9501b3`, 2026-09-19, `tests/authz_bypass_matrix_e2e.rs`) spawns `--actor-header X-Auth-Request-Email` and proves the one case a client can actually create behind `proxy_set_header` — a second copy of the header beside the edge's — is refused `400` with nothing recorded, while a form field named `actor` beside one good header changes nothing: the recorded `resolvedBy` is the header the server trusts. The same-origin, no-identity half is in `the_four_web_writes_are_refused_without_identity_and_across_origins_over_http`. |
| `SPA-13` | MUST | http | `the_served_pages_read_the_real_boards_and_write_to_none_of_them` | `the_json_routes_answer_get_only_over_http` extends it to the JSON surface (2026-09-19, `t-88814b7a`): `POST`, `PUT` and `DELETE` on `/api/v1/boards` answer `405` with a JSON body and `Allow: GET`, and the prefix is dispatched ahead of the POST arm in `route`, so no `/api/v1` path reaches the write handler at all. `no_page_can_reach_a_method_that_writes` holds the same boundary as a source-reading unit test, and `the_projection_reaches_nothing_but_the_store_and_the_registry` holds it over the projection with no allowlist. contract: `docs/api/kanban-web.openapi.yaml#/components/responses/MethodNotAllowed` — a JSON route answers `GET` and refuses every other method with `405` |
| `SPA-14` | MUST | chrome | `a_recommended_choice_resolves_in_one_click_in_real_chrome_and_records_its_outcome` | `the_pressed_answer_says_sending_on_its_own_fill_in_real_chrome` and `pressing_1_sends_and_advances_to_the_next_card_in_real_chrome` hold the in-flight label and the digit |
| `SPA-15` | MUST | chrome | `last_attention_card_task_drilldown_and_receipt_survive_websocket_refresh_in_real_chrome` | |
| `SPA-16` | MUST | chrome | `a_reply_typed_while_a_refresh_is_in_flight_is_not_discarded` | `a_choice_clicked_with_a_reply_records_the_note_in_real_chrome` proves the surviving text still records |
| `SPA-17` | MUST | chrome | `a_picked_verdict_survives_a_live_refresh_and_still_records_in_real_chrome` | |
| `SPA-18` | MUST | chrome | `a_click_shows_sending_until_the_board_answers_in_real_chrome` | |
| `SPA-19` | MUST | chrome | `an_incomplete_own_answer_refuses_before_posting_in_real_chrome` | `a_click_on_an_incomplete_custom_answer_says_what_is_missing_and_focuses_it` holds the focus move |
| `SPA-20` | MUST | chrome | `a_custom_answer_in_real_chrome_requires_an_outcome_and_records_one` | `enter_in_the_verdict_picker_records_the_free_text_answer_in_real_chrome` holds the keyboard commit |
| `SPA-21` | MUST | chrome | `a_refused_click_restores_the_card_in_real_chrome` | `a_refused_decision_brings_the_card_back_in_real_chrome` and `a_board_refusal_survives_more_typing_in_the_reply_in_real_chrome` hold the queue position and the surviving draft |
| `SPA-22` | MUST | chrome | `none` | no e2e coverage — to be written by `t-1f495a7f`; both shipped refusal cases stub a `409` at `window.fetch`, so no test lets the board refuse by name |
| `SPA-23` | MUST | chrome | `a_digit_in_the_verdict_picker_records_nothing_in_real_chrome` | `a_digit_after_tabbing_off_a_picked_verdict_records_nothing_in_real_chrome` and `a_digit_on_the_folded_answers_summary_records_nothing_in_real_chrome` hold the other two focus states |
| `SPA-24` | MUST | chrome | `a_picked_verdict_survives_a_live_refresh_and_still_records_in_real_chrome` | the same case presses `Escape` inside the card and asserts the verdict was released |
| `SPA-25` | MUST | chrome | `a_lagging_notice_socket_in_real_chrome_shows_one_summary_not_every_change` | `a_reconnected_notice_socket_in_real_chrome_does_not_replay_history` and `a_redelivered_notice_does_not_act_or_render_twice_in_real_chrome` hold reconnect and redelivery |
| `SPA-26` | MUST | chrome | `skip_moves_the_card_to_the_back_without_recording_in_real_chrome` | |
| `SPA-27` | MUST | chrome | `the_undo_key_bring_back_the_last_decision_in_real_chrome` | |
| `SPA-28` | MUST | chrome | `the_last_card_leaves_the_empty_state_in_real_chrome` | |
| `SPA-29` | MUST | chrome | `the_deck_answers_stack_and_the_note_is_reached_by_one_scroller_at_three_widths_in_real_chrome` | `the_deck_shows_one_card_and_only_its_body_scrolls_in_real_chrome` holds the single-scroller rule |
| `SPA-30` | MUST | chrome | `a_projection_swap_keeps_the_reader_where_they_were_in_the_card_in_real_chrome` | |
| `SPA-31` | MUST | chrome | `none` | no e2e coverage — to be written by `t-1f495a7f`; the shipped proof is `one_quiet_keys_line_carries_no_kbd_badges_unit` over served bytes the SPA retires. `needs_you_cards_take_a_note_a_keyboard_pick_and_a_deferral_in_real_chrome` and `the_custom_answer_is_folded_until_c_opens_it_in_real_chrome` observe the keys' behaviour but not the rendered line |
| `SPA-32` | MUST | chrome | `a_toast_stays_twenty_seconds_and_dismisses_in_real_chrome` | |
| `SPA-33` | MUST | chrome | `decided_receipts_collect_in_the_side_history_in_real_chrome` | `the_desk_is_two_columns_at_1280_and_a_drawer_at_820_in_real_chrome` holds the phone's history control |
| `SPA-34` | MUST | chrome | `the_live_line_and_the_toast_log_say_only_their_own_thing_in_real_chrome` | |
| `SPA-35` | MUST | chrome | `read_pages_are_rows_with_one_pill_and_a_mono_priority_in_real_chrome` | |
| `SPA-36` | MUST | chrome | `read_tables_are_borderless_but_for_the_hairline_in_real_chrome` | |
| `SPA-37` | MUST | chrome | `no_route_overflows_sideways_at_three_widths_in_real_chrome` | `render_answers_exactly_the_declared_shapes_unit` keeps the swept route list complete against `render`'s arms |
| `SPA-38` | MUST | chrome | `the_drawer_is_rows_on_the_desk_surface_in_real_chrome` | `the_current_page_is_marked_by_a_rule_in_real_chrome` holds the current-page rule |
| `SPA-39` | MUST | chrome | `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records` | the same journey enters a query and lands on `/search` |
| `SPA-40` | MUST | chrome | `reference_links_preview_on_hover_nest_and_open_a_new_tab_in_real_chrome` | |
| `SPA-41` | MUST | chrome | `markdown_renders_in_real_chrome_and_raw_html_stays_inert` | |
| `SPA-42` | MUST | chrome | `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records` | |
| `SPA-43` | MUST | chrome | `recent_decisions_page_lists_newest_first_and_undoes_in_real_chrome` | |
| `SPA-44` | MUST | chrome | `opening_a_draft_plan_in_real_chrome_moves_the_real_task_to_todo` | |
| `SPA-45` | MUST | chrome | `subscription_pause_and_resume_in_real_chrome_persist_each_state` | |
| `SPA-46` | MUST | chrome | `subscription_dead_letters_name_their_codes_in_real_chrome` | |
| `SPA-47` | MUST | chrome | `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records` | sprints index, board sprints and sprint detail; `no_route_overflows_sideways_at_three_widths_in_real_chrome` loads `/deployments` and `/deployment/{project}/{id}` with a seeded release |
| `SPA-48` | MUST | chrome | `a_cli_change_reaches_real_chrome_as_a_notice_without_a_reload` | |
| `SPA-49` | MUST | chrome | `a_lagging_notice_socket_in_real_chrome_shows_one_summary_not_every_change` | the summary rule; the no-body/no-token half has no browser test today and is written by `t-88814b7a` with SPA-09 |
| `SPA-50` | MUST | http | `none` | no e2e coverage — to be written by `t-bf255880`; the comparison is against ADR-048 §3's 1.32 MB baseline, which is an observation and not a budget |
| `SPA-51` | MUST | chrome | `none` | no e2e coverage — to be written by `t-1f495a7f`; `the_open_page_without_a_script_is_still_a_list_in_real_chrome_or_http` asserts WEB-56 and retires with it, as `every_deck_rule_is_scoped_to_a_page_whose_script_ran` does for WEB-58 |
| `SPA-52` | MUST | chrome | `nothing_is_boxed_in_real_chrome` | `the_recommendation_leads_on_fill_in_real_chrome`, `the_advance_runs_once_at_140ms_each_way_in_real_chrome`, `a_projection_refresh_never_reanimates_the_current_card_in_real_chrome` and `reduced_motion_advances_the_deck_without_animating_in_real_chrome` hold the fills and the one motion |
| `SPA-53` | MUST | chrome | `the_question_is_set_as_a_headline_in_real_chrome` | |
| `SPA-54` | MUST | chrome | `focus_is_visible_on_every_focusable_element_in_real_chrome` | `every_deck_control_is_forty_four_pixels_at_390_in_real_chrome` holds the 44 px floor |
| `SPA-55` | MUST | chrome | `the_card_is_named_by_its_question_in_real_chrome` | `every_field_is_labelled_and_status_is_announced_once_unit` holds the labelling over served bytes today |
| `SPA-56` | MUST | unit | `the_bundle_stylesheet_keeps_the_token_block_and_its_contrast_unit` | landed 2026-09-19 over the bundle's own stylesheet: its `:root` block is asserted equal to the served `CSS`'s, no hex is written outside it, and WEB-48/49/50's arithmetic re-runs on the bundle's tokens (the AA pair list is now one `AA_TOKEN_PAIRS` const both proofs read). Not re-run over the bundle yet, because the rules they judge have not moved into it: the `.pill` clause of `overlay_never_sits_on_surface0_unit`, and `no_heading_or_link_carries_a_glyph_prefix_unit`, `the_type_scale_is_declared_and_nothing_is_tracked_out_unit`, `the_stylesheet_names_one_serif_and_reserves_mono_for_code_unit` and `prose_blocks_are_bounded_to_seventy_characters_unit` — those arrive with the deck (`t-1f495a7f`, `t-bf255880`) |
| `SPA-57` | MUST | unit | `the_document_references_no_third_party_unit` | extended by `t-992e40aa` to sweep the bundle as well as the document |

**Counts.** 57 requirements, all `MUST`, no `SHOULD` and no `MAY`. By layer: 43 `chrome`, 6
`http`, 5 `unit`, 3 `process`. By group: identity 3 (`SPA-01`..`SPA-03`), readiness 2
(`SPA-04`..`SPA-05`), the JSON projection 8 (`SPA-06`..`SPA-13`), the Needs-you deck 21
(`SPA-14`..`SPA-34`), the read pages 8 (`SPA-35`..`SPA-42`), decided/plans/subscriptions/sprints
and deployments 5 (`SPA-43`..`SPA-47`), the live channel 2 (`SPA-48`..`SPA-49`), payload 1
(`SPA-50`), what is retired 1 (`SPA-51`), and the rendered result keeping ADR-046 6
(`SPA-52`..`SPA-57`).

**How the WEB figures below are counted.** A requirement *preserves* a `WEB-nn` when its
`Source` line says so; `SPA-51` is excluded because it retires two WEB requirements rather than
preserving any. On that rule **42** requirements preserve at least one `WEB-nn` — `SPA-10`,
`SPA-13`, `SPA-14`..`SPA-47` and `SPA-52`..`SPA-57` — and **36** of those also name, in the
table above, an existing Chrome test that observes the behaviour today: the six that do not are
`SPA-10` and `SPA-13` (proved at `unit` and `http`), `SPA-57` (proved at `unit`), `SPA-56`
(proved at `unit` over the bundle's stylesheet since 2026-09-19), and the two `none` rows
`SPA-22` and `SPA-31`.

**Rows with no evidence yet: 5.** `SPA-02`, `SPA-22`, `SPA-31`, `SPA-50` and `SPA-51`, each
naming the epic `e-9306a1d9` row that must write it. `SPA-51` is `none` because the test that
asserts today's behaviour is deleted with the surface, not re-pointed at it; `SPA-02` is `none`
because its remaining half is a host inventory rather than a test. Four rows left the list on
2026-09-19 with `t-992e40aa`: `SPA-03` (the `bundleSha256` receipt field and the installed
executable's `--version`), `SPA-04` and `SPA-05` (the product's mounted root, no longer a
fixture the test wrote), and `SPA-56` (ADR-046's token proofs re-run over the bundle's own
stylesheet). Three more left it the same day with `t-88814b7a`, which built the JSON
projection's first five routes: `SPA-06` (the queue asserted against the CLI over HTTP),
`SPA-07` (the capability the projection module does not hold) and `SPA-09` (the lease token
absent from both the detail and the index).

## 9. Change log

- `2026-09-19` — created at `Draft — gate requested`. `SPA-51` supersedes `docs/specs/web-ui.md`
  `WEB-56` and `WEB-58` by reference for the surfaces this slice serves, under ADR-048 §6; the
  `web-ui` specification itself is not edited by this slice.
- `2026-09-19` — `t-992e40aa` resolved OQ-1 and OQ-4 and landed the embedded bundle: §8's
  `SPA-01`, `SPA-03`, `SPA-04`, `SPA-05` and `SPA-56` now name shipped tests.
  `docs/testing/compiled-rust-e2e-matrix.md` carries the same rows verbatim.
- `2026-09-19` — `t-4b9501b3` added the INTEGRATION coverage of the JSON boundary over real
  HTTP (`tests/authz_bypass_matrix_e2e.rs` §9, named in `docs/api/README.md`): §8's `SPA-06`,
  `SPA-08`, `SPA-09`, `SPA-10`, `SPA-11` and `SPA-12` now name it beside their existing
  evidence, and `docs/testing/compiled-rust-e2e-matrix.md` carries the same rows verbatim.
  Three divergences from the contract were found and were held by `#[ignore]`d cases carrying
  their failing assertion, not by weakened ones: `/api/v1/lanes` serves an unauthorized
  board's sitreps, a whole-estate listing refuses entire rather than serving the readable
  subset, and the four write refusals answer `text/html` where the contract declares
  `application/json`. All three are recorded in `docs/api/README.md`.
- `2026-09-19` — `t-c84850a1` closed the first of those three: `Store::sitreps` takes the same
  `check_read(&[])` guard its sibling reads take, and `serve::lane_groups` skips a board whose
  sitreps read the store refuses instead of propagating it, so `/api/v1/lanes` and the `/lanes`
  page serve only the boards the principal may read. §8's `SPA-08` names the now-live
  `the_lanes_route_withholds_the_sitreps_of_a_board_the_caller_may_not_read_over_http`, its
  `#[ignore]` is gone, and `docs/testing/compiled-rust-e2e-matrix.md` carries the same row
  verbatim. Two divergences remain recorded in `docs/api/README.md`.
