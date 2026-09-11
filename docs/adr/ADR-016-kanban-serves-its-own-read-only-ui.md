# ADR-016: Kanban serves its own UI, read-only first, behind an edge it does not implement

**Status:** Accepted
**Date:** 2026-08-24
**Amended:** 2026-09-11 (decisions room: recent decisions, web undo, previews, markdown)
**Deciders:** George

## Context

Approvals are the bottleneck. `attention` records what needs the operator
durably and correctly ([ADR-012](ADR-012-session-handoffs-and-durable-attention.md)),
but settling one means being at a terminal with the right board addressed.
Measured on the day this shipped: **75 open items across 6 of the 13 boards**,
the oldest waiting 65 hours. None of that was visible anywhere at once — seeing
it required thirteen commands, one per project.

The plan for this was written as a draft epic (`e-ui`) and held there until
opened, which is the mechanism [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md)
exists for. Two questions were raised as an attention item rather than decided
by the agent that had them.

## Decision

### The server is in the binary, and calls the same store the CLI does

`kanban serve` renders pages from the same [`Store`] methods `kanban task list`
and `kanban attention list` call. A UI that reached past the store would be a
second implementation to keep in step — the drift ADR-010 and ADR-011 exist to
prevent, arriving through a third surface.

### `tiny_http`, decided 2026-08-24

Five crates total (`tiny_http`, `ascii`, `chunked_transfer`, `httpdate`,
`log`), blocking, no async runtime. That matches a codebase which is
synchronous throughout because rusqlite is.

`axum` was the alternative and was rejected: it brings tokio, tower and hyper —
roughly eighty crates — and an async rewrite of every store call. Hand-rolling HTTP on
`TcpListener` would have kept the dependency count at zero and meant
maintaining an HTTP parser on an internet-facing surface, which is not a trade
worth making for a page that renders tables.

The 2026-08-26 live-status amendment does not reverse that stack decision.
`tiny_http` exposes the standards-compliant HTTP upgrade boundary. A small
thread per connected operator tab owns the upgraded socket; `sha1` and `base64`
produce the RFC handshake. The synchronous Store remains synchronous and no
async runtime enters the ledger.

### It binds loopback, and there is no flag to change that

Kanban implements no authentication. It binds `127.0.0.1` and trusts the edge.
That edge is now the shared Google SSO for `*.geoy.ws`, restricted to
`geoyws@gmail.com`; nginx also forwards the original Host for same-origin write
validation.

**There is deliberately no `--bind` flag.** Its only correct value is the
default, and any other value publishes an unauthenticated surface to the
network. A flag whose wrong setting is catastrophic and whose right setting is
what you get by not passing it is not a flag, it is a footgun. Fronting this
for remote access is the documented arrangement, not a workaround.

Stated plainly, because it is the real risk: **whoever controls the allowed
Google account can resolve operator attention.** For a single-operator tool
that is the intended authority. OAuth credentials, cookies and authorization
headers never enter a rendered page or WebSocket message.

### Read routes write nothing; the two write routes are allowlisted

The e2e serves every page and compares the board file byte-for-byte before and
after. That proves no page *did* write. It cannot prove no page *could*: a
mutating call that happens to be a no-op on the day leaves the bytes identical
and the capability in place — which was demonstrated, not assumed, by injecting
a `sweep_expired_claims` call that the byte comparison passed straight over.

So a second guard reads the module back and checks all 23 of `Store`'s
`&mut self` methods. It allowlists three method names — `resolve_attention`,
`resolve_attention_from_trusted_edge`, and `move_task` — while the two shipped
browser capabilities are trusted-edge attention resolution and draft opening.
The compiled-binary E2E separately proves that cross-origin, malformed and
duplicate submissions do not mutate a board.

### Write scope, decided 2026-08-24: two verbs, both approval-shaped

Resolve an attention item with a reply; open a draft (`draft` → `todo`), which
is what releases a plan's work. Both are now shipped. Plan opening accepts only
an existing draft epic, uses the same audited `Store::move_task` operation as
the CLI, and refuses cross-origin or duplicate submissions. It cannot move a
task, story, or non-draft epic. Everything else stays read-only. Full board
control would be a much larger surface to build, design and secure, and every
verb in it becomes reachable with the password.

The actor for a UI write is `geoyws` by default — `model::OPERATOR_ACTOR`,
which is the single definition `serve` reads. It was written here as `geo`
until 2026-09-06; the ledger's own resolve/reopen gate now accepts the literal
`geoyws` and refuses the ambiguous `geo`, so an ADR still naming `geo` was
documenting an actor its own board would reject. An opt-in `--actor-header NAME`
path can replace that default when a trusted edge injects a validated header
value; the edge must strip any client-supplied copy and set
`X-Auth-Request-Email` from a successful `auth_request`. If the header name is
invalid, missing, duplicated, empty, oversized, or malformed, the write fails
closed. Same-origin still applies, and `--actor-header` may be enabled only
while the server remains loopback-only. If this ever has a second user that
stops being true, it needs revisiting before then rather than after.

### Needs you is live, but WebSockets are not a second ledger

The 2026-08-26 requirement changed from occasional refreshes to live operator
status. `/live` therefore upgrades to a WebSocket after the same-origin check.
Frames contain only `ready`, `refresh`, a non-sensitive revision fingerprint,
and heartbeats. They contain no task or attention body, credential, cookie,
lease token, or write capability.

The server fingerprints the registered SQLite database, WAL and rollback
journal file states. When one changes, the browser fetches and swaps in the
canonical server-rendered projection. An answer in progress blocks that swap
— words typed into a reply or a verdict picked for a free-text answer, both
of which the re-rendered projection would come back empty — and the block is
re-checked when the projection arrives, because an answer can be started
while it is in flight. This preserves one read path and makes the socket a
notification channel rather than replicated state.

Every hold is releasable, because one that is not is a frozen page: the card
carries a `Clear verdict` control, shown exactly while a verdict is picked,
and `Esc` inside the card does the same. HTML offers no other way to
un-check a radio group. Typed words are never cleared for the operator —
losing them is what the hold exists to prevent — so the release takes the
verdict and the composer's own refusal and nothing else; what the board
refused stays on the card until another attempt replaces it.

Clicking an authored choice releases the picker too, before the body is
built. An authored choice carries its own verdict and the route forwards no
picker value onto it, so a verdict the operator happened to leave picked is
not part of that decision — and a card that went on showing it would be
claiming a verdict the ledger does not carry, which is exactly what the
operator would check the card to find out.

The two refusals a card can show are separate lines with separate voices,
because they are separate claims. The composer's own is the card declining to
post an answer it can see is half-written, and it speaks the page's language:
pick a verdict, write your reply. What the route or the network said is
quoted verbatim, flags and all, because the operator may have to act on the
exact words. Neither can overwrite the other's line: one shared line meant a
pre-flight refusal could take the board's sentence and the next keystroke
could then clear it as the composer's own, leaving a card that looked
unrefused with nothing recorded anywhere.

The hold is page-wide and the release is per-card, and that asymmetry is real
rather than an oversight: the swap replaces the whole `<main>`, so there is
nothing narrower than the page to hold, while the only honest place for a
`Clear verdict` button is beside the verdict it clears. A verdict picked on a
card that is then scrolled out of view therefore holds the whole page's
projection with its own release off screen. What is on screen in that state is
the page-wide live line reading `update waiting`, for as long as the hold
lasts; the way back is to reach that card and release it there.

What no keystroke from outside the verdict picker can do is overwrite a
verdict the operator picked. Inside it the browser's own keys still apply —
the arrow keys move the verdict and `Space` picks one, which is how a radio
group is operated — and `Esc` releases it, as above. The digits need a focused
card to answer at all, and any field that takes text keeps them; on top of
that they are inert on a card whose verdict picker has a checked radio — a
property of the CARD rather than of whatever has focus inside it, because one
`Tab` from the picker lands on the submit and a digit pressed there would
otherwise reach the recommendation. `Enter` in the picker is aimed at that
card's own submit for the same reason: the browser's implicit submission would
pick the form's first submit button, which is that same recommendation. A
typed reply alone does not disarm the digits, deliberately — a reply rides
with whichever choice is clicked, which is what the field above it says it
does — so `1` on a card with words typed, no verdict picked and the cursor
outside the reply field records the recommendation and sends those words with
it.

Reply forms are bounded and strictly decoded. Browser POSTs require the Origin
authority to equal Host and all browser attention resolution calls
`Store::resolve_attention_from_trusted_edge`: default mode supplies
`OPERATOR_ACTOR` (`geoyws`), while
opt-in actor-header mode supplies the trusted edge value. The CLI uses
`Store::resolve_attention`. Empty, oversized, malformed, cross-origin,
unknown-board and already-resolved submissions fail without a partial write.

### No hot reload

ADR-011's in-place `execve` works because a stdio server holds nothing and
inherits one pipe. An HTTP server holds accepted connections. This one is
*restartable* instead — it keeps no state, so updating is `install` then
`systemctl restart`. The trick does not transfer and is not claimed to.

That restart does **not** interrupt the agents using Kanban. CLI operations are
short-lived processes which open the SQLite ledger directly, and MCP tool calls
spawn that same installed binary; the long-lived MCP protocol process has
ADR-011's in-place replacement. `kanban serve` is only the browser-facing web
surface. At worst, a browser request arriving during the brief restart can fail
and be retried; claims, leases, checkpoints and other ledger operations remain
available throughout.

Do not add application-level hot reload to remove that browser-only gap. If the
web surface later requires uninterrupted availability, solve that at the HTTP
process boundary with socket activation or a draining handover, not by importing
the stdio reload mechanism into a server with accepted connections.

### A server is not an operation

`mcp` and `serve` block until killed, which makes them meaningless as MCP tool
calls — the adapter spawns the binary and reads its result, so a tool that never
returns hangs the caller. They are excluded from the generated tool list by a
named set, `LONG_RUNNING`, and the manifest publishes the property so a consumer
can tell without knowing the names.

This was a bare `!= "mcp"` inside the tool builder. It was correct while there
was one such command and wrong the moment there were two, which is what a
literal in place of a set always eventually is.

## Amendment, 2026-09-11: the decisions room

George commissioned this batch in one sitting, in his words: "when a decision
has been made we need to put the item in a recent decisions tab or something
so that the eye can easily engage the next item"; "hotkeys to make it easy to
select and confirm and undo to bring back the last item that was decided on";
"all reference links... must allow for mouseover to show what they are and
also allow for nested mouseovers and if clicked should open a tab to that
item"; "use formatting and markdown and etc to make it easier to read as well
for all our texts"; and on the skin, that the phosphor-neon terminal look is
not mandatory and the page may wear the OMP harness's palette instead.

**The undo is a third write shape, on purpose.** `POST
/attention/<board>/<id>/reopen` reopens exactly one decided item through the
same audited `Store::reopen_attention` the CLI's `attention reopen` uses,
gated exactly like the reply route: same-origin, the trusted-edge actor
(default `geoyws`), the store's operator-or-resolver gate, and a fixed reopen
note (`undone from the web view`) because an undo that demanded words would
be a dialog wearing a button. It joins `move_task` and the subscription verbs
in the module's write-guard allowlist. The reply route's refusals are
inherited: cross-origin is a 403, an already-open row is a 409 carrying the
store's own words, an unknown board a 404.

**Recent decisions is a read view, `/decided`.** The newest resolved items
across every board (each board scanned newest-first to a 200-row bound,
merged, truncated to 20), each rendered with its question, its decision in
the ledger's own words, its note, who decided and when, and one Undo. The
keyboard rule is the same everywhere: `u` reopens the decided row under
focus, and on Needs you it falls back to the newest receipt on the page —
the one the last keypress just made. After an undo the projection refreshes
and focus lands on the brought-back card, so `1`–`4` keep working.

**Hover previews are a read route, `/preview/<kind>/<board>/<id>`.** The
page script derives the URL by prefixing `/preview` to a reference anchor's
own path, so task, board, deployment and attention references all preview
without a second mapping. The fragment it returns is not a page: a whole
document inside a document would bring a second socket and a second copy of
the keyboard map into being behind the operator's back. Anchors are marked
`data-ref` and open `target=_blank rel=noopener`; the fragments themselves
render `data-ref` anchors (a task preview names its parent), which — with
every listener delegated to the document — is what makes previews nest.

**Markdown renders board texts, and raw HTML never survives it.** Bodies,
notes, plan bodies and sitrep bodies go through `pulldown-cmark`
(`default-features = false, features = ["html"]` — the crate's first entry in
the dependency list since the server was chosen), with `Html`/`InlineHtml`
events dropped and link destinations restricted to `http(s)`, `mailto` and
same-page anchors. A soft break renders as a hard break: bodies were
`pre-wrap` plain text before, and a receipt's SHA line or a `RESOLVE-WHEN`
clause is line-shaped on purpose. Scalar interpolation still goes through
`escape`; the parser is the one bounded place a body meets one.

**The skin is the OMP harness's, not a terminal's.** The phosphor green, the
CRT overlays and the webfont are gone; the palette follows the
`dark-catppuccin-omp` theme installed in George's omp (crust `#11111b`,
base `#1e1e2e`, text `#cdd6f4`, blue `#89b4fa`, green `#a6e3a1`, red
`#f38ba8`, peach `#fab387`), the system sans stack reads the prose and mono
is reserved for ids, keys and receipts. One register with the chat the
decisions are made from, so the page and the harness do not read as two
products.

**Acceptance is compiled-process and real-Chrome, per this repo's rule.**
`tests/e2e.rs` pins: the undo key round trip (receipt gone, card back with
focus, row open, `decision` cleared, `attention_reopened` carrying the
previous decision), `/decided` newest-first with an undo from the page, the
reopen route's cross-origin/open-row/unknown-board refusals, hover previews
opening and nesting with Escape closing them, and markdown rendering with raw
HTML inert — plus the unit tests for the renderer itself.

## Consequences

Live at `https://kb.geoy.ws`; `systemd` keeps it up, and the unit, vhost and their
`init.sh` lines are all in the dotfiles, so a rebuild reproduces them.

The unit failed on its first start with `HOME is not set` — systemd sets no
`HOME`, and the data root is derived from it. The binary refused rather than
guessing a directory, which is the behaviour that made the cause obvious from
one log line instead of from a board written somewhere nobody expected.

The styling is structural, not a design. Phase 3 is the `/frontend-design`
pass, once there is something real to look at; shipping plain markup first is
deliberate, and the markup is semantic so that pass has a clean skeleton.

Not offered, and each for a reason: no arbitrary SQL, ever (ADR-001 §6); no
multi-user accounts or roles (drivers are identities that claim work, not people who log in); no
editing plan bodies in a browser (a plan is markdown that belongs in a commit,
and a textarea is the wrong tool for it).

The accepted availability trade is therefore explicit: deploying the web view
may create a momentary read-only browser error, while the agent-facing ledger
path stays available. That is not a reason to add hot reload unless the web
view's availability requirement changes.

Deliberately not decided: what happens when one operator becomes two. Every
choice above — one actor, one password, no sessions — is correct for one person
and wrong for two, and the point to revisit is when that changes rather than in
anticipation of it.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) §6 — narrow operations, never arbitrary write SQL
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — one description of the surface
- [ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md) — the reload this deliberately does not inherit
- [ADR-012](ADR-012-session-handoffs-and-durable-attention.md) — the attention items this page exists to surface
- [ADR-013](ADR-013-plans-are-epics-and-drafts-are-not-yet-work.md) — the draft epic that held this plan until it was opened
