# The operator web API

`kanban-web.openapi.yaml` is the contract for the JSON projection the embedded
operator SPA reads, and for the four writes it may make. This file says how to
read it, what it deliberately does not cover, and — in the appendix — which
OWASP ASVS requirements were applied to it and which were excluded and why.

Both files answer `docs/specs/spa.md` §7 **OQ-3** — *"What are the JSON routes
called, and how are they versioned?"* — which `SPA-06` deliberately left open.

**This is a contract, not a report.** At the baseline commit `cd55cbc` the
server serves HTML from `render` (`rust/serve.rs:421`-`rust/serve.rs:470`) and
**no `/api/v1` route exists**. `t-88814b7a` implements the projection against
this document; `t-992e40aa` implements the bundle that consumes it. Nothing
here claims a shipped behaviour except where it cites the line that ships it.

## How to read the document

- **`openapi: 3.0.3`, pinned.** Not 3.1. The reader is a reviewer with
  whatever tooling is on their machine, and 3.0.3 is the version every such
  tool reads. 3.1's only gain here would be full JSON Schema 2020-12, which
  none of these flat scalar-and-array records need. Newest is not a
  requirement; readable by the reader is. The file says the same thing in its
  own header comment so the choice survives being read alone.
- **Start with `x-requirement`.** Every operation carries the `SPA-nn`
  identifiers from `docs/specs/spa.md` §3 that it satisfies. Spec → route →
  test is one hop: the same IDs index §8 of the specification and the SPA
  section of `docs/testing/compiled-rust-e2e-matrix.md`.
- **Then read `x-store-method`.** ADR-048 §5 binds the API to being a *thin
  projection over the same `Store` methods the CLI and MCP call* — no new SQL,
  no second query implementation. `x-store-method` names the methods the page
  arm being mirrored already calls today, each verifiable with
  `grep -n 'pub fn <name>(' rust/store.rs`. An operation that wants data no
  named method exposes is a request for a `Store` method, decided on the
  ledger's terms; it is not a licence to write a query in the web layer
  (`SPA-07`).
- **`x-registry-method` is the honest exception.** Two operations read the
  registry as well as the store: `getBoard` for the board's applicable rules,
  and `search` for the rules corpus. Rules are registry state, not board state,
  and no `Store` method exposes them. They are recorded under their own
  extension rather than folded into `x-store-method`, because a document that
  pretended they came from the store would be wrong about the one thing ADR-048
  §5 cares about.
- **Every schema field name is the serialised name of a real Rust field.**
  `rust/model.rs` structs carry `#[serde(rename_all = "camelCase")]` and, where
  the id reads badly in camelCase, explicit renames — `taskID`, `agentID`,
  `sessionID`, `consumerID`, `actionID`, `sprintID`, `parentID`, `type`. Three
  places had to choose a name rather than read one, and each says so in its own
  `description`: `SubscriptionPosition` and `DeadLetterCode` have no
  `Serialize` derive today, and `BoardSummary` / `LaneSummary` / `AttentionCard`
  are pairings the page computes inline and writes straight into HTML. Those
  names are fixed *here* so the projection has one to implement rather than one
  to invent.
- **Absent is not null.** `DeploymentAttempt.sprintID`, `.targetVersion`,
  `.servedVersion` and `SearchResult.taskID` carry
  `skip_serializing_if = "Option::is_none"`, so they are missing from the
  object rather than `null`. Everything else that is optional is `nullable`.

## Validating the document

```
python3 -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" docs/api/kanban-web.openapi.yaml
npx --yes @redocly/cli@latest lint docs/api/kanban-web.openapi.yaml
```

The lint is clean, with four warnings that are **expected and must not be
"fixed"**: `operation-2xx-response` fires on each of the four POSTs, because
each answers `303` and none answers a `2XX`. That is the shipped behaviour —
`WebResponse::Redirect` renders a `303` with a `Location`
(`rust/serve.rs:343`-`rust/serve.rs:350`) — and documenting a `200` that the
server does not send to silence a linter would make the contract wrong in
order to make a tool quiet.

Two further checks belong to whoever edits this document, and are what the
`x-` extensions exist for:

```
# every x-requirement must be a requirement that exists
grep -o 'SPA-[0-9][0-9]' docs/api/kanban-web.openapi.yaml | sort -u
# every x-store-method must be a method that exists
grep -n 'pub fn <name>(' rust/store.rs
```

One `x-store-method` is `pub(crate)` rather than `pub` and will not match a
`pub fn` grep: `Store::resolve_attention_from_trusted_edge`
(`rust/store.rs:6064`). That is deliberate — the trusted-edge resolve is
unreachable outside the binary and the web edge is its only caller, while
`Store::resolve_attention` (`rust/store.rs:6051`) is the public sibling the
CLI uses.

## Boundaries — what this contract does not cover

- **The CLI grammar.** `kb`'s commands, flags, exit statuses and `--json`
  shapes are their own contract (ADR-010, ADR-008) and are not restated here.
  The API calls the same `Store` methods the CLI calls; it does not wrap the
  CLI and it is not generated from it.
- **The MCP tool surface.** ADR-011's in-binary MCP server keeps its own tool
  schemas. A change to this document is not a change to those.
- **The edge.** Authentication, sessions, TLS, public reachability and the
  Google SSO belong to nginx and oauth2-proxy (ADR-016). Exactly one thing
  crosses the boundary into this document: the `X-Auth-Request-Email` header,
  described under `securitySchemes: trustedEdge`.
- **`/live`.** The WebSocket keeps its four frame kinds — `ready`, a notice,
  `refresh`, `heartbeat` — unchanged in kind by this slice (`SPA-48`,
  ADR-048 §5). It is not an HTTP operation and OpenAPI 3.0 cannot describe it
  honestly, so it is not described here at all rather than described badly.
  Its content rule is the one thing this document depends on and restates:
  no body, no credential, no lease token, no write capability (`SPA-49`).
- **The bundle's bytes.** `/app` and `/assets/{file}` are listed under the
  `bundle` tag so a reader knows the whole served surface, but they carry no
  JSON, read nothing from the ledger, and are `t-992e40aa`'s to build.

## Compatibility at this boundary

The version in `/api/v1` is a **compatibility boundary, not a release
number**. It does not move when the product does.

- **Additive is free.** A new field on a response object, a new optional query
  parameter, a new enum member in a field the client already treats as an
  opaque string, a new route: all of these stay `v1`. A client that ignores
  what it does not recognise keeps working, and every client here is a client
  we ship in the same binary.
- **Removing or renaming a field is a version bump.** It lands under
  `/api/v2`, with `/api/v1` kept until the bundle that reads it is gone —
  which, since the bundle ships inside the same executable, is exactly one
  release. A rename dressed as an addition plus a deletion is a rename.
- **A narrowed type is a removal.** Making a nullable field non-nullable,
  shrinking an enum, or tightening a bound a client may already have observed
  is a breaking change wearing a smaller hat.
- **The four writes never move at all.** Their paths, their method, their
  form encoding and their field names are pinned by `SPA-10` and ADR-016's
  allowlist. They are outside `/api/v1` precisely so that versioning the read
  surface can never be mistaken for permission to reshape the write surface.

## Listing bounds

Every listing answers in the `ListEnvelope` shape: `{items, returned, limit,
truncated}`.

ADR-037 decided that a CLI listing which exceeds a default the caller never
set **refuses**, and named option `A` — this exact envelope — as the migration
path for the one case a refusal cannot serve: a consumer that needs the partial
rows *and* the fact that they are partial, in one response (ADR-037 §4). The
browser is that consumer. `SPA-06` requires the page's data to arrive; a page
handed a refusal instead of rows is a blank screen where a bounded list
belongs.

This envelope is adopted for the JSON surface only. It does not migrate the CLI
listings, which keep ADR-037 §3's silence under an explicit `--limit` and §2's
refusal of an exceeded default. If those listings ever adopt `A`, this is the
same shape and the two agree by construction.

Two rules make the flag worth reading:

1. **`truncated` is computed, never inferred.** The listing asks the store for
   one row past the bound it will return and sets the flag from whether that
   row came back — the `context_packet` discipline of ADR-037 §1. A result with
   exactly `limit` rows and no extra is complete, and says so.
2. **`limit: null` means genuinely unbounded.** Several page arms call store
   methods that take no limit at all — `Store::list_tasks`,
   `Store::sprint_tasks`, `Store::current_deployments`, and
   `Store::sprints(None, true, i64::MAX)`. Those listings report `limit: null`
   and `truncated: false` rather than inventing a number. A listing that
   reports a bound it does not have is a lie an operator would plan around.

The bounds that do exist are the page's own, and each operation names the line
it reads them from: 1000 open attention rows per board, a 200-row decided scan
cut to 20, 200 sitreps per board, 100 started deployments and 30 of each
failure status per board, 50 notes / checkpoints / events on a task, 30 search
results.

## The denial does not enumerate

There is one error shape — `{"error": "<sentence>"}` — and one sentence for
every refusal that could otherwise confirm existence:

> `denied or not found`

This is the store's own single generic denial, `DENIED_OR_NOT_FOUND`
(`rust/authz.rs:28`-`rust/authz.rs:33`), reused verbatim rather than
paraphrased. It is byte-identical whether the row is invisible to the caller,
absent from the board, or on a board that does not exist, because a refusal
that distinguishes those is an existence oracle. No board name, row title, tag,
retirement note or count appears in it, on any route, for any principal.

The same rule shapes enumerations: a caller who **names** one row is told
`denied or not found`; a caller who asks for a **list** is simply not handed
the rows they may not see, because a refusal in the middle of an enumeration
says "there is something here you cannot have", which is exactly what the
single generic denial exists to avoid (`rust/authz.rs:180`-`rust/authz.rs:189`).
A retired board is absent from `/api/v1/boards` and from its own route
(`SPA-08`).

Two refusals are deliberately *not* generic, and both are the product's own
words rather than the store's state:

- `403` — `The action did not come from this site.` (`rust/serve.rs:497`). The
  same-origin gate names the class of problem, not the row.
- `409` — the store's own sentence on a write that the board refused (already
  resolved, a choice key the row no longer carries). These name the state of a
  row the operator just acted on and is authorised to see; they enumerate
  nothing they could not already read.

## JSON API integration

The boundary this document describes is exercised against the **compiled
binary over real HTTP**, on a real SQLite estate, in
`tests/authz_bypass_matrix_e2e.rs` — section 9, "The JSON projection over real
HTTP" — alongside `tests/access_refusals_e2e.rs`, which holds the compiled
refusal matrix the estate's enforcement states rest on. The layer is `http`:
the process boundary is real and no browser is involved, so these are
INTEGRATION cases and are never labelled otherwise (rule `g-ffbd95f5`).

They live in that file rather than in one of their own because the fixture
does: `ManagedEstate` is the only fixture in the suite that can put the
*serving* process under managed enforcement with a named set of grants, which
is what the authorization cases need and what the first wave's cases in
`tests/e2e.rs` could not seed. Every case names the `SPA-nn` it answers in its
doc comment, and `docs/specs/spa.md` §8 and the SPA section of
`docs/testing/compiled-rust-e2e-matrix.md` name the case back.

What they hold, in the order this document states it: board and tag
authorization equal to the CLI's on all five routes, with the CLI read run as
the same identity against the same estate; the one non-enumerating body, and
every `4xx` searched for the invisible board's name, row titles, ids, tag and
lane; no lease token in any of the five responses after a real `kanban claim`;
every listing's `ListEnvelope` with all three bounds actually crossed; the
four writes refused with no trusted-edge identity and refused cross-origin;
a second, client-supplied copy of `X-Auth-Request-Email` refused rather than
preferred; a reply naming a choice the row no longer carries refused by name;
and the **projection invariant** — the four read routes compared row for row,
field by field on shared keys, against `task list`, `task show`, `sitrep list`
and `attention list --status open`, so a future divergence fails there instead
of being discovered in the UI.

Three cases in that section are `#[ignore]`d and carry a failing assertion on
purpose. Each is a divergence named below rather than a test written down to
the shipped behaviour.

## Where the served pages do not yet meet this contract

Recorded here because the specification's §8 evidence rows will be read against
it, and because a contract that quietly diverges from the shipped surface is
worse than one that names the gap.

- **A named board that is unknown, retired or ambiguous is `500` today on the
  pages, with detail — closed on the JSON surface by `t-88814b7a`; the page
  arms still answer `500`.** `board(name)` calls `project_named`, whose error
  text names the board and, for a retired one, its retirement note
  (`rust/registry.rs:571`-`rust/registry.rs:581`); `handle` renders that text
  into a `500`. The shipped test asserts exactly this: `/board/RETIRED`
  returns `500` and the body contains the retirement note `retire served
  board` (`serve_hides_retired_boards_from_the_board_index_and_board_route`).
  The JSON surface does not copy it: `/api/v1/board/{project}` and
  `/api/v1/task/{project}/{id}` answer `404` with the byte-identical
  `{"error":"denied or not found"}` for an unknown, retired or ambiguous
  board and for an absent row, asserted by
  `the_json_projection_refuses_unknown_retired_and_unauthorized_boards_with_one_body_over_http`.
  The pages keep their behaviour until the cutover (`t-1f495a7f`) retires
  them, so this line stays a divergence rather than a closed item.
- **kanban sets no security response headers.** The headers the server adds
  are `Content-Type` — `text/html; charset=utf-8` on a page,
  `application/json; charset=utf-8` on a JSON route — plus `Location` on a
  redirect, `Cache-Control: public, max-age=31536000, immutable` on a
  content-hashed bundle asset, and `Allow: GET` on a JSON `405`. There is no
  `Content-Security-Policy`, `X-Content-Type-Options` or `Referrer-Policy`
  anywhere in `rust/`. Whatever CSP `kb.geoy.ws` has is the edge's. The
  appendix records this as an open item against V3 and V14 rather than as a
  claim.
- **Five of the fifteen JSON routes exist.** `t-88814b7a` implemented the
  first wave in `rust/projection.rs`: `/api/v1/needs-you`, `/api/v1/boards`,
  `/api/v1/board/{project}`, `/api/v1/lanes` and
  `/api/v1/task/{project}/{id}`. The other ten — `/api/v1/decided`,
  `/api/v1/sprints`, `/api/v1/sprints/{project}`,
  `/api/v1/sprint/{project}/{id}`, `/api/v1/plans`, `/api/v1/deployments`,
  `/api/v1/deployment/{project}/{id}`, `/api/v1/subscriptions`,
  `/api/v1/search` and `/api/v1/preview/{kind}/{project}/{id}` — are
  unimplemented, and they answer the same non-enumerating `404` as an unknown
  board rather than a "not implemented" that would inventory what is coming.
  `SPA-06`, `SPA-07` and `SPA-09` carry their first evidence from this wave;
  the remaining routes stay owed by the epic.
- **`/api/v1/lanes` serves the sitreps of a board the caller may not read.**
  Found 2026-09-19 by `t-4b9501b3`. `lane_groups` (`rust/serve.rs`) scans
  every *active* board and calls `Store::sitreps`, which carries no guard, so
  a board that `kanban sitrep list` refuses to the same principal is handed
  over in full over HTTP — lane, author, body and worktree path. This
  contradicts `SPA-08` and this document's own rule that a caller is never
  handed rows they may not see. Held by the `#[ignore]`d
  `the_lanes_route_withholds_the_sitreps_of_a_board_the_caller_may_not_read_over_http`,
  whose positive control is the CLI refusing the same read. Closing it is a
  change to the store or to the scan, not to the test.
- **A whole-estate listing refuses entire rather than serving the readable
  subset.** Found 2026-09-19 by `t-4b9501b3`. `board_summaries` and
  `needs_you` iterate every active board and propagate the first refusal, so
  `/api/v1/boards` and `/api/v1/needs-you` answer `404 denied or not found` to
  a caller who fully owns one board out of two. The CLI's `dashboard` answers
  identically, so the two surfaces agree and `SPA-08`'s equivalence holds —
  but "The denial does not enumerate" above says a caller who asks for a list
  is *simply not handed* the rows they may not see, and `SPA-06` requires the
  page's data to arrive. Held by the `#[ignore]`d
  `a_whole_estate_listing_serves_the_boards_the_caller_may_read_over_http`.
- **The four writes refuse in HTML; this contract says they refuse in JSON.**
  Found 2026-09-19 by `t-4b9501b3`. `Refused`, `WriteRejected` and
  `WriteConflict` each declare `application/json; charset=utf-8` with the
  `Error` schema, and all four `POST` operations reference them; the server
  renders a `text/html` page for every one, carrying the same sentence inside
  the markup. The browser deck depends on that page — it parses the response
  and reads `.error` — so this one may well be the document's to fix rather
  than the server's, but it is a divergence either way. Held by the
  `#[ignore]`d `the_write_refusals_answer_the_contracts_json_error_body_over_http`.

---

# Appendix: OWASP ASVS applicability

**Selected version: OWASP ASVS 5.0.0** (dated May 2025). The SDD reference this
project writes under names the ASVS repository README as the authority for
which version is current, and records 5.0.0 as the latest stable version when
reviewed on 2026-09-10
(`/Users/geoyws/.agents/skills/quality/references/spec-driven-development.md:182`).
The README still identified 5.0.0 as latest stable when re-read for this
document on 2026-09-19.

**ASVS is a requirements source and a verification aid. It is not a
certification claim.** Nothing in this appendix asserts that this interface is
ASVS-verified at any level, by anyone. It states which requirements were used
to shape the contract and which were excluded, with reasons — which is the
obligation the SDD reference actually imposes
(`…/spec-driven-development.md:83`).

**A numbering note, because it will trip a reader holding older material.**
ASVS 5.0.0 renumbered its chapters wholesale. The familiar 4.0.3 numbers —
V2 authentication, V3 session management, V4 access control, V5 validation,
V8 data protection, V13 API, V14 configuration — are *different chapters* in
5.0.0. The table below uses 5.0.0's numbering, which is the version selected;
this map is for anyone reading a 4.0.3-shaped brief beside it:

| Theme | 4.0.3 chapter | 5.0.0 chapter |
| --- | --- | --- |
| Authentication | V2 | **V6** Authentication |
| Session management | V3 | **V7** Session Management |
| Access control / authorization | V4 | **V8** Authorization |
| Validation, sanitization, encoding | V5 | **V2** Validation and Business Logic, **V1** Encoding and Sanitization |
| Data protection | V8 | **V14** Data Protection |
| API and web service | V13 | **V4** API and Web Service |
| Configuration | V14 | **V13** Configuration |

Requirement identifiers below are written in the ASVS project's recommended
versioned form, `v5.0.0-<chapter>.<section>.<requirement>`.

## Selection table

| 5.0.0 chapter | Verdict | Requirements applied | Reason |
| --- | --- | --- | --- |
| **V1** Encoding and Sanitization | Applicable | `v5.0.0-1.2.3` | Responses are built by `serde_json` over typed structs, never by string concatenation, so JSON structure cannot be changed by content. The *rendering* half — agent-authored markdown displayed as text, never as HTML — is `SPA-41`, and it is the client's obligation now that the client builds the DOM. |
| **V2** Validation and Business Logic | Applicable | `v5.0.0-2.1.1`, `v5.0.0-2.2.1`, `v5.0.0-2.2.2` | Every path and query parameter carries a documented type and bound in the document (2.1.1). The one free-text input, `search?q=`, is trimmed and bounded, and an empty query returns the empty set rather than every row (2.2.1). All validation is in the Rust handler and the store, never in the client (2.2.2): the browser is a convenience, not a control. |
| **V2** Anti-automation §2.4 | N/A | — | One operator, one loopback listener, no registration, no public surface. Rate limiting a single-user tool that binds `127.0.0.1` protects nothing and adds a failure mode. |
| **V3** Web Frontend Security | Partly applicable | `v5.0.0-3.2.1`, `v5.0.0-3.4.2`, `v5.0.0-3.5.1`, `v5.0.0-3.5.3` **applied**; `v5.0.0-3.4.3`, `v5.0.0-3.4.4` **open** | Every JSON response is `application/json; charset=utf-8` and never `text/html`, so a body cannot be rendered in the wrong context (3.2.1). No `Access-Control-Allow-Origin` header is emitted at all, which is the strongest fixed value available (3.4.2). State-changing requests are `POST` and are gated on `Origin == Host` rather than on CORS preflight (3.5.1, 3.5.3, `SPA-11`). **Open:** the server sets no `Content-Security-Policy` and no `X-Content-Type-Options: nosniff` — verified absent from `rust/` — so 3.4.3 and 3.4.4 are satisfied only to whatever extent the nginx edge supplies them, which this document does not control and will not claim. |
| **V3** Cookie setup §3.3 | N/A | — | kanban sets no cookie, on any route. The edge's oauth2-proxy cookie is the edge's, and it never reaches a response body or a frame (ADR-016:66-:67). |
| **V4** API and Web Service | Applicable | `v5.0.0-4.1.1`, `v5.0.0-4.1.3`, `v5.0.0-4.1.4`, `v5.0.0-4.4.1`, `v5.0.0-4.4.2` | Every response declares a `Content-Type` matching its body with an explicit charset (4.1.1). **4.1.3 is the trusted-edge clause exactly**: `X-Auth-Request-Email` is set by an intermediary and cannot be overridden by the end user, because nginx uses `proxy_set_header`, which overwrites any client copy (ADR-016:98-:105). JSON routes answer `GET` and refuse every other method with `405` (4.1.4, `SPA-06`). `/live` runs over WSS at the edge (4.4.1) and its handshake is `Origin`-checked before the upgrade (4.4.2, ADR-016:107-:110). |
| **V4** GraphQL §4.3 | N/A | — | There is no GraphQL endpoint. ADR-040 considered one and it is not this surface. |
| **V5** File Handling | N/A | — | The API accepts no upload and serves no user-supplied file. The only bytes served that are not JSON are the bundle's own assets, compiled into the executable (`SPA-01`). |
| **V6** Authentication | N/A, with reason | — | **kanban implements no authentication and there is no flag to change that** (ADR-016). The Google SSO at the edge authenticates; a contract for the operator UI cannot restate the edge's posture and must not pretend to own it. The single named residual risk is recorded in ADR-016:64-:67: whoever controls the allowed Google account can resolve operator attention, and for a single-operator tool that is the intended authority. |
| **V7** Session Management | N/A, with reason | — | **There are no sessions.** No session token is issued, accepted, stored or terminated by kanban; the browser holds no durable state beyond the current view and a reload reconstructs everything from the projection (`docs/specs/spa.md` §5). Session lifetime at the edge is oauth2-proxy's. |
| **V8** Authorization | Applicable | `v5.0.0-8.1.1`, `v5.0.0-8.2.1`, `v5.0.0-8.2.2`, `v5.0.0-8.3.1` | The rules are documented — board scope and all-of-tag, both capabilities — in `rust/authz.rs:35`-`rust/authz.rs:79` and ADR-038 clause 5 (8.1.1). They are enforced **inside the store**, not by a filter in the route (8.3.1, ADR-048 §5, `SPA-08`): a route adds no second rule of its own, and a route that filtered would be a control a future caller could forget. Data-level access is per row and per tag, which is the IDOR/BOLA case 8.2.2 names (8.2.1, 8.2.2). Same-origin (`SPA-11`) is CSRF defence and grants no capability; it is listed under V3, not here, because it authorises nothing. |
| **V9** Self-contained Tokens | N/A | — | No JWT, no signed cookie, no self-contained token of any kind is issued or accepted by this interface. |
| **V10** OAuth and OIDC | Out of scope | — | kanban is not an OAuth client, resource server or authorization server. oauth2-proxy at the edge is, and its configuration is not this document's. |
| **V11** Cryptography | N/A to this interface | — | The projection performs no cryptographic operation. The ledger's hash-chained audit trail is ADR-029's and is served as opaque `prevHash`/`eventHash` strings. |
| **V12** Secure Communication | Edge-owned | — | TLS terminates at nginx. The listener binds loopback in plaintext by design and has no `--bind` flag, precisely so it cannot be published unauthenticated (ADR-016:51-:62). |
| **V13** Configuration | Partly applicable | `v5.0.0-13.4.1`, `v5.0.0-13.4.2`, `v5.0.0-13.4.5` **applied**; `v5.0.0-13.3.x` **N/A** | The binary reads no source-control metadata at request time and serves every asset from its own bytes (13.4.1, `SPA-01`). There is no debug mode and no hot reload; a new bundle arrives by restart (13.4.2, ADR-016). This document and the matrix are files in the repository, never served endpoints, and there is no monitoring endpoint (13.4.5). **13.3 secret management is N/A**: the interface holds no secret. `Subscription.secretRef` is an opaque host-local *lookup name*, never a credential value (`rust/model.rs:148`-`rust/model.rs:149`), and resolves to nothing in a browser. |
| **V14** Data Protection | Applicable | `v5.0.0-14.2.1`, `v5.0.0-14.2.6`, `v5.0.0-14.3.3` **applied**; `v5.0.0-14.3.2` **open** | No identifier, key or token is carried in a URL or query string — the one query parameter in the whole read surface is `search?q=` (14.2.1). **The minimum-data rule is structural, not a filter**: a task's holder is served as `ClaimSummary` (`rust/model.rs:559`), which has no `lease_token` field, rather than as `Claim` (`rust/model.rs:501`), which does at `:508`. A filter can be forgotten; a type cannot (14.2.6, 14.3.1 in the same spirit, `SPA-09`). Nothing sensitive is written to browser storage because nothing sensitive is ever served (14.3.3). **Open:** the server sets no `Cache-Control: no-store`, so 14.3.2 depends on the edge. |
| **V15** Secure Coding and Architecture | Applicable | `v5.0.0-15.3.1` | "Return only the required subset of fields" is the same obligation as `SPA-09` and ADR-048 §5's "the API never serves a lease token", and it is met by choosing narrower types rather than by redacting wider ones. The dependency-currency requirements of §15.1/§15.2 are the repository's, not this interface's, and are not claimed here. |
| **V16** Security Logging and Error Handling | Applicable (error handling) | `v5.0.0-16.5.1` | One error shape, one generic sentence, no stack trace, no query, no key, no token — and the deliberate reuse of the store's own `denied or not found` so that "not yours" and "not there" are indistinguishable. The **logging** half of V16 is not claimed: kanban's durable record is the hash-chained event ledger (ADR-029), which is a different artefact answering a different question, and no security-event log inventory exists for the web surface. |
| **V17** WebRTC | N/A | — | No WebRTC. |

## Exclusions, restated plainly

Five chapters are excluded outright, and each for a reason that is a property
of this system rather than a deferral:

1. **V6 Authentication** — kanban implements none; the edge owns it (ADR-016).
2. **V7 Session Management** — there are no sessions, no tokens and no logout.
3. **V9 Self-contained Tokens** and **V10 OAuth/OIDC** — nothing here issues,
   accepts or validates a token; the estate has no bearer token at all.
4. **V5 File Handling** and **V17 WebRTC** — the surfaces simply do not exist.

Three requirements are recorded as **open** rather than met, because the
server does not set the headers they ask for and this document will not claim
an edge configuration it does not own: `v5.0.0-3.4.3` (CSP),
`v5.0.0-3.4.4` (`nosniff`) and `v5.0.0-14.3.2` (`Cache-Control: no-store`).
Closing them is a change to `rust/serve.rs`'s response headers, which is
`t-88814b7a`'s surface, not this row's.

## Sources

- `docs/specs/spa.md` — the slice specification; §3 group "The JSON projection"
  (`SPA-06`..`SPA-13`), §5 contracts, §6 quality and security, §7 OQ-3.
- `docs/adr/ADR-048-the-operator-ui-is-a-typescript-spa-embedded-in-the-binary.md`
  §5 — the binding thin-projection rule.
- `docs/adr/ADR-016-kanban-serves-its-own-read-only-ui.md` — loopback, the
  trusted edge, the write allowlist, the `/live` frame-content rule.
- `docs/adr/ADR-037-truncated-listings-refuse-a-default-limit-they-exceed.md`
  §1, §3, §4 — computed truncation and the `A` envelope.
- `rust/authz.rs` — the single generic denial and the non-enumerating rule.
- `rust/model.rs`, `rust/store.rs`, `rust/serve.rs` — the structs, the methods
  and the arms every schema and operation is derived from.
- OWASP ASVS 5.0.0 — <https://github.com/OWASP/ASVS/tree/v5.0.0>.
