# ADR-016: Kanban serves its own UI, read-only first, behind an edge it does not implement

**Status:** Accepted
**Date:** 2026-08-24
**Amended:** 2026-08-26
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
canonical server-rendered projection. A draft reply blocks that swap so an
agent update cannot erase text George is typing. This preserves one read path
and makes the socket a notification channel rather than replicated state.

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
