# UI event-consumption and notification UX seams

**Epic:** `e-c5418a31`, under `e-f907debf` (UI consumers for canonical pub/sub
task events). **Date:** 2026-09-06. **Status:** mapping only — no consumer is
built here. Its siblings `e-a9e8241f` (browser notification consumer) and
`e-8bb7e07e` (subscription preferences and cursor presentation) are the
building.

Everything below was read out of the tree at `92ae2f3`, not recalled.

## The two channels that already exist, and why they are not the same thing

There are two live push mechanisms and they answer different questions. The
whole risk in this epic is answering one with the other.

### The revision socket — "something changed, ask again"

`serve.rs::websocket` upgrades a same-origin `GET`, then loops once a second
over `ledger_revision()`:

| Frame | When |
|---|---|
| `{"type":"ready","revision":"<16 hex>"}` | on connect |
| `{"type":"refresh","revision":"<16 hex>"}` | the revision changed |
| `{"type":"heartbeat"}` | ~15 idle ticks, so a dead socket is detectable |

`ledger_revision()` hashes, for every active project, the name, the board path,
and the `stat` of the board file plus its `-wal` and `-journal` siblings. WAL
mode updates the `-wal` inode rather than the main file, so all three
participate.

Three properties follow, and they are the reason this channel is safe:

1. **It carries no ledger content and no capabilities.** The frame is a number.
   The browser's only reaction is to re-fetch the page it is already on, which
   goes through the normal authorized read path.
2. **It has no cursor and needs none.** A missed `refresh` is harmless: the
   next one supersedes it, and a reconnect starts from `ready`. There is no
   position to lose and nothing to replay.
3. **It is deliberately coarse and deliberately unauthorized.** One number for
   the whole estate. It is computed before any authorization context exists,
   and it tells a listener only that *something, somewhere* changed.

### The watch envelope — "here is which row changed, and where you were"

`watch.rs` emits `WatchEnvelope { version, scope, cursor, type, payload }`,
where `scope` is the full `ScopeEnvelope` (source kind and path, board name,
selector kind and value, kinds, relations, prior and current statuses, tags,
archived) and `cursor` is a base64 `CursorToken` carrying that same selector
plus a `seq`.

This channel is the opposite of the first on every axis: it names rows, it is
ordered, it is resumable, and since `da21b6b` it is **authorization-filtered**
per delivery — `events_since_filtered` runs every row through `visible_events`,
and the authority is re-minted once per poll so a revocation lands mid-stream.

## The seam rule

> A surface may consume the revision socket freely. A surface may consume
> watch envelopes only through the server, and must never hold a cursor in the
> browser.

Two reasons, and the second is the one that bites.

**Duplicating ledger semantics.** A cursor is a claim about what you have
already seen. Held in a browser it becomes a claim the server must trust, and a
stale or replayed one silently skips rows — the failure mode is missing
information that looks like absence of information, which is exactly the class
`e-df5fcee3` ("read surfaces must not let absence read as a finding") closed
elsewhere. The ledger already owns "what have I seen": `seq`. Nothing in the
browser needs a second copy.

**Duplicating authorization.** The revision socket is safe *because* it is
coarse. Make it per-board — an obvious-looking optimisation, so that a page
only refreshes when its own board moves — and it stops being a notice and
becomes an authorization surface: the frame then discloses which board is
active to a listener whose authority was never checked, because
`ledger_revision()` runs over `projects_active()` with no guard and no
`AuthzContext`. Any per-board or per-row push must therefore go through
`authz::check_read` first, which means going through the server, which means it
is a watch envelope and not a revision frame.

## Surface by surface

| Surface | Today | Channel it should consume | Why |
|---|---|---|---|
| **Needs you** (`serve.rs:833 needs_you`) | full render per request; the socket triggers a re-fetch | revision only | Its content is "open attention items", a small authorized query. A refresh notice plus the existing read is already correct and already authorized. A per-item push would need the guard and would disclose item existence. |
| **Task detail** (`serve.rs:1513 task_detail`) | full render per request | revision only, filtered client-side to the open task's board | The page is one row and its trail. It cannot act on an envelope it is not allowed to read, and it has no ordering requirement — the newest render wins. |
| **Plans** (`serve.rs:1285 plans`) | full render, `?opened=` | revision only | Same shape as Needs you. Tree state lives in the URL, not in a stream. |
| **Search** (`serve.rs:928 search_page`) | full render, `?q=` | **neither** | Results are ranked and query-scoped. A refresh mid-typing is hostile, and re-ranking on every ledger write would be worse than stale results. Let the operator re-run the query. Since `da21b6b` search filters per hit against tags re-read live from `task_tags`/`attention_tags`, so a result set is authorized at materialisation — but that is a reason it is *safe*, not a reason to push it. |
| **Notifications** (`e-a9e8241f`) | does not exist | watch envelopes, server-side | This is the one surface that genuinely needs per-row identity: a notification names a thing. It must be a server-side consumer holding the cursor and the `AuthzContext`, pushing only an already-authorized summary to the browser. |
| **Subscription preferences** (`e-8bb7e07e`) | does not exist | neither | Preferences are board rows, edited through the normal authorized write path. |

## What this mapping decides, so the siblings do not each decide it

1. Four of the five existing surfaces need **no new channel at all** — the
   revision socket plus their existing authorized reads is the whole answer.
   Search needs nothing.
2. The only new consumer is the notification one, and it is **server-side**.
   The browser receives summaries, never envelopes, and never a cursor.
3. `ledger_revision()` stays coarse. If a future change makes it per-board or
   per-row, that change is an authorization surface and must take an
   `AuthzContext` and call `authz::check_read` — it is not a performance tweak.
4. Cursor presentation (`e-8bb7e07e`) means *showing* a cursor's meaning, not
   storing one in the client.

## Open, and not decided here

- Whether the notification consumer is a thread inside `kanban serve` or a
  separate process. `serve` is loopback-only and single-operator today
  (ADR-016); a second process would need its own `AuthzContext`, which is
  cheap since `da21b6b`, but also its own lifecycle. `e-a9e8241f`'s call.
- The revision socket polls at 1s and heartbeats at ~15 idle ticks. Whether a
  notification consumer reuses that cadence or subscribes properly is
  `e-a9e8241f`'s call; this document only forbids it from inventing a second
  cursor.
