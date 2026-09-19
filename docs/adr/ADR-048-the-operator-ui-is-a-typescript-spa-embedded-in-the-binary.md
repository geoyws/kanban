# ADR-048: The operator UI is a TypeScript SPA, built at build time and embedded in the binary

**Status:** Accepted
**Date:** 2026-09-17 (recorded 2026-09-19)
**Deciders:** George decided the shape on 2026-09-17, after two rounds of measurement and one
reversal of his own (the React Native / App Store branch he raised and withdrew the same day).
He owns the shape and its scope. The wording of this document is decided by codex@driver;
George may supersede any clause of it.
**Supersedes:** nothing wholesale. It **amends** [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md):
the server-rendered-projection swap of its "Needs you is live, but WebSockets are not a second
ledger" section and the inlined-asset arrangement implicit in "The skin is the OMP harness's"
are superseded by §1 and §5 below. Everything else in ADR-016 stands, unedited, as §7 lists.

**On the number.** The planning brief for this ADR said 047. ADR-047 is the SDD rollout
(`/Users/geoyws/work/src/kanban/docs/adr/ADR-047-kanban-adopts-specification-driven-development.md`,
written 2026-09-18) and ADR-046 the web design system, so this decision is 048.

## Context

`kb.geoy.ws` is served by `kanban serve`, which renders HTML from the same `Store` methods the
CLI calls, and ships its entire client as two Rust string constants compiled into the one
executable. At `30b433f` those constants are `JS` at `rust/serve.rs:3865` and `CSS` at
`rust/serve.rs:5103`.

George raised the rebuild on 2026-09-17 *after* measuring the server, not before, and then
raised and withdrew a mobile-native branch the same day. Epic `e-9306a1d9` carries the
measurements; this ADR records the decision they led to, what they did and did not justify, and
the constraints the rebuild is held to. ADR-047 §7(b) already names `SPA` as the first slice
specified under the SDD conventions, so this ADR decides the architecture and `t-eed0a923`'s
specification decides the requirements.

## Decision

### 1. The operator UI becomes a React + TypeScript single-page application, embedded in the binary

The server-rendered pages of ADR-016 are replaced, for the operator UI at `kb.geoy.ws`, by a
React + TypeScript single-page application. The application is compiled and bundled at **build
time** on a build host, and the resulting bundle is **embedded in the one Rust executable** —
the same way `JS` and `CSS` are embedded today, and with the same consequence: what `hax` and
`hig` serve is still the compiled binary's own bytes.

Nothing Node-, Bun-, npm-, pnpm-, Yarn- or Corepack-shaped runs on `hax` or `hig`, and nothing
of that shape is needed to run the served artefact. [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md)
and rule `r-cf2b2b9f` are unchanged in substance; the rule is scoped to `RUNTIME` with a
build-time exception for this epic, which ADR-047's Consequences already record —
`docs/adr/ADR-047-kanban-adopts-specification-driven-development.md:207`-`:222`
("no Node, Bun, npm, pnpm, Yarn or Corepack on `hax` or `hig`, and none needed to run the served
artefact — with a **build-time exception for the `SPA` epic**", and the honest statement of the
tension with ADR-046 §3(a) beneath it).

### 2. Why: type safety, state handling, and headroom — in George's words

George's reasons, 2026-09-17: **TypeScript type safety** and **React state handling**, in place
of JavaScript held in Rust string constants; and **headroom** — a growing surface and heavier
use than today's. He was explicit that 7.2 CPU-seconds at near-idle personal use is not the
ceiling he is planning for.

The size of the thing being replaced, recorded twice and honestly, because it grew while the
decision was being written up:

- **648 lines** of inlined script when George decided, on 2026-09-17.
- **1217 lines** at `30b433f` on 2026-09-19: `const JS` spans `rust/serve.rs:3865`-`rust/serve.rs:5081`,
  61,785 characters of JavaScript as the epic records it (re-measured at `30b433f`, the const
  block is 61,828 bytes on disk including its `const JS: &str = r#"` and `"#;` lines).
- Beside it, **419 lines** of stylesheet: `const CSS` spans `rust/serve.rs:5103`-`rust/serve.rs:5521`,
  26,968 characters as recorded (26,993 bytes for the block on disk at `30b433f`).

Nearly doubling in two days is the argument. The `web-ui` specification records the same two
constants as an observation and refuses to make them a budget
(`docs/specs/web-ui.md:580`-`:584`);
this ADR does not make them one either. It reads them as a trajectory.

### 3. What was measured before deciding, and what it does not justify

From epic `e-9306a1d9`, `kanban-serve` on `hax`, 2026-09-17:

- **0.0% CPU**, **7.2 CPU-seconds** total (5.0 user + 2.2 sys) over **7054 s** of uptime,
  **RSS 79 MB**.
- `/attention`: **0.35 ms** at **46 KB**.
- `/boards`: **54 ms** at **53 KB**.
- `/`: **55 ms** at **1.32 MB**.

The 54 ms is SQLite reads across 24 boards. That cost is paid identically whether the answer is
rendered as HTML or serialised as JSON, so moving the rendering to the client does not remove it.

**Stated plainly: this change does not reduce measured server CPU, and was not chosen for that.**
It is chosen for §2's reasons. Anyone who later reads this ADR as a performance decision is
reading it wrong.

The **1.32 MB landing payload is a real defect** — the one number above that is wrong rather
than merely large — and it is in this epic's scope as `t-bf255880`. It is a defect of the
current page, not an argument for a new architecture; a server-rendered `/` could be fixed
without this ADR.

### 4. Alternatives, and why each lost

**(a) Keep the inlined JavaScript, add JSDoc type-checking only.** Rejected: type annotations in
comments over a 1217-line string constant buy checking without buying component state, and are
insufficient for the surface George is planning for. The constant's growth in §2 is the evidence.

**(b) Author TypeScript and emit it into the binary, without React.** Rejected for the same
reason: it buys the type safety of §2's first clause and none of its second. Server-rendered
pages plus progressive-enhancement script stay the shape, and the state handling George named
stays hand-rolled.

**(c) React Native plus an App Store client.** Raised by George on 2026-09-17 and **withdrawn by
him the same day**. Recorded here as considered and withdrawn, because the two gates it would
have forced are worth naming so nobody re-raises it without them:

1. **Real authentication in a ledger that has none of its own.** ADR-016's "It binds loopback,
   and there is no flag to change that" is explicit — "Kanban implements no authentication. It
   binds `127.0.0.1` and trusts the edge"
   (`docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:51`-`:67`). A phone app cannot be
   handed a loopback socket; it would need the product to grow accounts, sessions and tokens.
2. **Public reachability** for a private single-operator tool that today sits behind Google SSO
   restricted to one account (same clause). An App Store client needs a publicly reachable
   endpoint, which is the exact surface `--bind`'s absence exists to prevent.

Anything mobile-native remains undecided, not rejected on merit; the Consequences say so.

### 5. Binding: the JSON API is a thin projection over the same Store, or it is not built

The browser-facing JSON API is a second **surface**, and it is permitted only as a **thin
projection over the same `Store` methods the CLI and MCP call**. Concretely, and this is binding
on the specification and on the implementation:

- **No new SQL.** No query written for the browser that the CLI does not already run.
- **No second query implementation**, and no reaching past the store. This is ADR-016's own rule —
  "`kanban serve` renders pages from the same [`Store`] methods … A UI that reached past the
  store would be a second implementation to keep in step — the drift ADR-010 and ADR-011 exist
  to prevent, arriving through a third surface"
  (`docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:26`-`:31`). A JSON encoder in front
  of a store call satisfies it; a handler with its own `SELECT` does not.
- **Authorization parity with the CLI.** The same board and tag authorization, enforced in the
  store rather than in the route. The API **never serves a lease token**. Writes keep
  same-origin gating (Origin authority equals Host,
  `docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:177`-`:183`) and keep the trusted-edge
  actor: default `OPERATOR_ACTOR` (`geoyws`), or the validated `X-Auth-Request-Email` value under
  opt-in `--actor-header` (`docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:94`-`:105`).
- **`/live` is unchanged in kind.** It keeps carrying only revision notices, readiness and
  heartbeats — "no task or attention body, credential, cookie, lease token, or write capability"
  (`docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:107`-`:113`) — plus authorized
  summaries where the SPA needs them. It does not become replicated state.

What §1 supersedes in that section is only the **mechanism**: the browser no longer "fetches and
swaps in the canonical server-rendered projection". It fetches JSON and re-renders. The
in-flight-answer hold that wording protects is not superseded; the Consequences carry it as
`t-1f495a7f`.

### 6. Accepted costs, each named so it is never a surprise

- **The no-JavaScript fallback is lost.** Today every decision form posts without script: the
  `web-ui` specification has a whole "Without a script" section — WEB-56, "with scripting off,
  `/` is a plain list that still works", and WEB-58, every deck rule scoped to `html.js`
  (`docs/specs/web-ui.md:416`-`:433`),
  with WEB-38's scriptless navigation at `docs/specs/web-ui.md:405`-`:409` and the A10 scenario
  at `docs/specs/web-ui.md:668`-`:672`. The served markup also renders the
  [ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) §5 card order —
  question, context, recommended, alternatives, custom answer, folded body, meta
  (`docs/adr/ADR-042-attention-items-are-decision-cards-with-authored-choices.md:463`-`:481`) —
  so that a scriptless page reads correctly. A mounted application cannot keep that. The SPA
  spec retires those requirements explicitly under ADR-047 §3 supersession, rather than letting
  them rot as passing tests of a surface that no longer exists.
- **A second read surface exists.** The JSON API, held to §5 and to nothing looser.
- **The single-artifact release proof gains a second half.** `readlink /proc/<MainPID>/exe`
  alone no longer proves what is served. The current practice — the unit active with a `MainPID`
  "whose executable resolves inside the `releases/<id>` this run activated, within a 15 s
  deadline", then a 200 from the unit's own port
  (`docs/adr/ADR-039-release-manifest-and-receipt-schema.md:490`-`:493`, and the incident that
  produced it at `docs/adr/ADR-034-hig-release-packages-and-board-rule-transfer.md:83`-`:106`) —
  proves the executable. The deploy receipt must now prove **both** the executable and the
  **bundle fingerprint** it carries, tied to the measured build provenance of
  [ADR-044](ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md) §2,
  whose `manifestSha256` and `toolchain`/`buildKind`/`builderImage` fields are the place a
  bundle's build environment is recorded
  (`docs/adr/ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md:118`-`:154`).
  An embedded bundle whose provenance is not in the receipt is an unproven artefact.
- **The SDD gate applies first.** ADR-047 requires the slice to be specified before it is
  implemented: `t-eed0a923` reaches `SPEC-READY` first, and `SPEC-READY` authorises neither
  implementation nor release
  (`docs/adr/ADR-047-kanban-adopts-specification-driven-development.md:170`-`:183`). WEB's
  real-Chrome contracts are **re-proven on the SPA**, not assumed to carry over.

### 7. What ADR-016 keeps, and exactly what it does not

**Unchanged, all of it**, at ADR-016's post-amendment line numbers: the server is in the binary
and calls the `Store` (`:26`-`:31`); `tiny_http`, five crates, blocking, no async runtime
(`:33`-`:49`); loopback only, no `--bind`, the SSO edge and the stated single-account risk
(`:51`-`:67`); the write-guard allowlist over `Store`'s `&mut self` methods (`:69`-`:82`); the
two approval-shaped write verbs of 2026-08-24 — now three with the 2026-09-11 reopen amendment
(`:84`-`:92`, `:229`-`:238`); no hot reload, restart instead (`:185`-`:203`); a server is not an
operation (`:205`-`:215`); the decisions room, previews and markdown rendering of the
2026-09-11 amendment as *product behaviour*.

**Superseded, precisely two things:**

1. The **projection-swap wording** in "Needs you is live, but WebSockets are not a second ledger"
   (`docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:107`-`:122`, supersession note now
   at `:124`) — specifically "the browser fetches and swaps in the canonical server-rendered
   projection". Superseded by §1 and §5: the browser fetches JSON and re-renders. The frame
   content rule, the in-flight-answer hold and its releasability are kept.
2. The **inlined-asset arrangement** implicit in "The skin is the OMP harness's"
   (`docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md:269`-`:276`, note at `:278`) and
   realised as the `CSS` and `JS` constants in `rust/serve.rs` (`rust/serve.rs:3865`,
   `rust/serve.rs:5103`). Superseded by §1: the assets become a built bundle. The *palette and
   register* that clause decides — the `dark-catppuccin-omp` theme, sans for prose, mono for
   ids and receipts — are not superseded; ADR-046 owns them and the SPA wears them.

## Consequences

**The test seam already landed.** `tests/e2e.rs` carries one selector module and one readiness
function so the cutover is one edit rather than forty-three: `mod ui` at `tests/e2e.rs:26171`
with `SHELL` (`:26177`) and `APP_ROOT` (`:26181`, "Not served yet -- the mount is t-eed0a923's"),
`wait_for_app_root` at `tests/e2e.rs:26324`, and `wait_for_shell_ready` at `tests/e2e.rs:26362`,
which the comment at `:26359`-`:26361` says "becomes `wait_for_app_root(tab, ui::APP_ROOT)` and
nothing else in the file has to know". Landed as commit `c6efcd7`, *"test(chrome): one selector
module and one readiness seam for a client-mounted page"*.

**The slice's gates, in order:** `t-eed0a923` (the `SPA` specification — the first gate, and
`SPEC-READY` before any implementation), `t-19d4c16d` (the API contract), `t-88814b7a` (the JSON
projection under §5), `t-992e40aa` (the embedded bundle and its build step), `t-1f495a7f` (the
Needs-you cutover, preserving in-flight answers), `t-bf255880` (the remaining pages, retiring
the server-rendered ones, and fixing the 1.32 MB landing payload of §3).

**Deliberately not decided here:** the bundler and the React version — `t-eed0a923`'s
specification decides them, under ADR-047 §6 — and anything mobile-native, which §4(c) records
as withdrawn rather than settled.

## References

- Epic `e-9306a1d9` (the `SPA` rebuild, and the source of §3's measurements); task `t-aed0352a`
  commissioned this ADR; rule `r-cf2b2b9f` (`RUNTIME`: no Node/Bun/npm on `hax` or `hig`, with
  the build-time exception for this epic); attention `a-180f934d` — the join-after-release gate,
  already superseded by ADR-047, which is why this ADR is written before the slice ships
- [ADR-016](ADR-016-kanban-serves-its-own-read-only-ui.md) — the decision this amends; §7 lists
  what it keeps and the two clauses it loses
- [ADR-006](ADR-006-rust-runtime-and-compiled-binary-e2e.md) — the Rust runtime and the
  compiled-binary e2e rule, unchanged in substance by §1
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md),
  [ADR-011](ADR-011-in-binary-mcp-server-and-in-place-reload.md) — the drift the §5 projection
  constraint exists to prevent
- [ADR-042](ADR-042-attention-items-are-decision-cards-with-authored-choices.md) §5 — the card
  order the scriptless page preserves today
- [ADR-044](ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md) §2 —
  measured build provenance, which the bundle now rides in
- [ADR-046](ADR-046-the-web-ui-is-one-designed-system.md) — the design system the SPA wears
- [ADR-047](ADR-047-kanban-adopts-specification-driven-development.md) — the SDD rollout; §7(b)
  names `SPA` and its Consequences scope the runtime rule with the build-time exception
- `docs/specs/web-ui.md` — the current slice's requirements, including the "Without a script"
  section §6 accepts losing
