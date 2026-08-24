# ADR-016: Kanban serves its own UI, read-only first, behind an edge it does not implement

**Status:** Accepted
**Date:** 2026-08-24
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
roughly eighty crates — and an async rewrite of every store call, to buy
streaming and websockets this page does not use. Hand-rolling HTTP on
`TcpListener` would have kept the dependency count at zero and meant
maintaining an HTTP parser on an internet-facing surface, which is not a trade
worth making for a page that renders tables.

### It binds loopback, and there is no flag to change that

Kanban implements no authentication. It binds `127.0.0.1` and trusts the edge —
`auth_basic` in the nginx already on this box, the pattern `dash`, `atmux` and
`docs` all use.

**There is deliberately no `--bind` flag.** Its only correct value is the
default, and any other value publishes an unauthenticated surface to the
network. A flag whose wrong setting is catastrophic and whose right setting is
what you get by not passing it is not a flag, it is a footgun. Fronting this
for remote access is the documented arrangement, not a workaround.

Stated plainly, because it is the real risk: **basic auth in front of a write
surface means whoever holds the password can approve anything.** For a
single-operator tool that is an acceptable trade. The vhost strips
`Authorization` before proxying, so the credential never reaches the binary.

### Phase 1 writes nothing, and that is enforced twice

The e2e serves every page and compares the board file byte-for-byte before and
after. That proves no page *did* write. It cannot prove no page *could*: a
mutating call that happens to be a no-op on the day leaves the bytes identical
and the capability in place — which was demonstrated, not assumed, by injecting
a `sweep_expired_claims` call that the byte comparison passed straight over.

So a second guard reads the module back and asserts it names none of `Store`'s
twenty `&mut self` methods. Phase 2's two writes go in its allowlist with a
reason, which is a decision someone makes on purpose rather than a check that
quietly stops applying.

### Write scope, decided 2026-08-24: two verbs, both approval-shaped

Resolve an attention item with a note; open a draft (`draft` → `todo`), which
is what releases a plan's work. Everything else stays read-only. Full board
control would be a much larger surface to build, design and secure, and every
verb in it becomes reachable with the password.

The actor for a UI write is `geo`. There is one person behind that password, so
the attribution is honest. If this ever has a second user that stops being
true, and it needs revisiting before then rather than after.

### No hot reload

ADR-011's in-place `execve` works because a stdio server holds nothing and
inherits one pipe. An HTTP server holds accepted connections. This one is
*restartable* instead — it keeps no state, so updating is `install` then
`systemctl restart`. The trick does not transfer and is not claimed to.

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

Live at `https://kb.geoy.ws`, five routes, 90 KB and 27 ms for the full
75-item landing page. `systemd` keeps it up; the unit, the vhost and their
`init.sh` lines are all in the dotfiles, so a rebuild reproduces them.

The unit failed on its first start with `HOME is not set` — systemd sets no
`HOME`, and the data root is derived from it. The binary refused rather than
guessing a directory, which is the behaviour that made the cause obvious from
one log line instead of from a board written somewhere nobody expected.

The styling is structural, not a design. Phase 3 is the `/frontend-design`
pass, once there is something real to look at; shipping plain markup first is
deliberate, and the markup is semantic so that pass has a clean skeleton.

Not offered, and each for a reason: no websockets or live updates (a refresh
suffices for a page opened a few times a day, and it keeps the server
synchronous); no arbitrary SQL, ever (ADR-001 §6); no multi-user accounts or
roles (drivers are identities that claim work, not people who log in); no
editing plan bodies in a browser (a plan is markdown that belongs in a commit,
and a textarea is the wrong tool for it).

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
