# ADR-011: An in-binary MCP server that runs the CLI, and replaces itself in place

**Status:** Accepted
**Date:** 2026-08-21
**Deciders:** George

## Context

[ADR-001](ADR-001-durable-agent-work-ledger.md) §6 says consumers receive narrow
operations rather than arbitrary write SQL, and that MCP adapters expose the
same operations the CLI does.
[ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) made the
surface readable as data so an adapter could be generated from it instead of
restating it, and deliberately left open whether Kanban ships a server itself.

It should. An adapter maintained elsewhere is a second thing to install, version
and keep in step with the binary it drives, and the moment it lives in another
repository the drift ADR-010 exists to prevent comes back through the side door.
The manifest makes an external adapter *possible*; shipping the server makes it
*unnecessary*.

The second question is what happens when the binary is updated while a server is
running. A stdio MCP server is a subprocess of its client: the client spawned it
and holds the pipe, so its lifetime is not ours to manage. Replacing the file on
disk leaves the running process on the old, now-unlinked inode, serving stale
code until something makes the client restart it — and a restart is exactly the
disturbance an operator updating a tool does not want to cause.

## Decision

### A tool call runs the binary

Each `tools/call` spawns the executable and lets the ordinary CLI parse,
validate and answer it. The MCP layer builds an argument list and reads the
result; it does not reach into the store.

This costs a process spawn per call, a few milliseconds against operations that
touch SQLite anyway, and buys the property that matters: there is no second code
path. Validation cannot diverge because there is only one. The refusal an agent
reads is the refusal an operator reads, which is worth more here than in most
places, because this codebase spends its refusals telling the caller what to do
instead — and an adapter that paraphrased them would throw that away.

A command that fails is returned as a tool result marked `isError`, carrying its
stderr, rather than as a JSON-RPC error. The message is the most useful thing an
agent can be given, and a transport-level error would bury it in a layer the
model never sees.

An argument the operation does not declare is refused before anything is
spawned, mirroring the unknown-flag rule
([ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)).
Passing it through would only relocate the same refusal somewhere less obvious.

### The server replaces itself in place

The server holds nothing between requests: durable state is in SQLite and the
protocol is strict request/response, so the process is idle and empty the moment
it has answered. That is what makes an in-place swap safe. `execve` replaces the
process image while the process id and the open file descriptors survive, so
from the client's end of the pipe nothing happened at all.

Three conditions gate it, and each corresponds to a way the swap could be
noticed:

- **Only between requests.** Swapping mid-request drops a reply a client is
  blocking on.
- **Only with an empty read buffer.** `execve` keeps descriptors and discards
  memory, so a request that had been read but not yet parsed would vanish with
  the old image — and the client would wait forever for an answer to something
  the server did receive. This is why the server buffers input by hand rather
  than through a convenience reader whose buffer it cannot inspect.
- **Only after the replacement proves it runs.** Exec'ing a broken build takes
  the pipe down with it, which is indistinguishable from a crashed server. The
  candidate is run once first; if it fails, the old image keeps serving and says
  so on stderr, which is not the protocol channel.

Because the check runs before the server blocks on the next read, a replacement
that appears while it is blocked is acted on at the end of the following turn —
one request later. That lag is real and is not worth removing: tool calls spawn
the binary from disk, so **operations are already running the new code from the
first call after the update**. Only the protocol layer and the tool list wait
for the swap, and neither changes in a typical update.

The server does not require `initialize` before serving. Insisting on it would
make a reload visible: the new image did not witness the handshake, and would
reject a client that is, correctly, not going to repeat it.

## Consequences

`kanban mcp` is a complete MCP server with no new dependencies. The transport is
newline-delimited JSON-RPC over stdio, which needs nothing beyond `serde_json`
and the standard library, and the tool list is the ADR-010 manifest.

An update is `install` over the binary, as it always was. Running servers pick it
up without a client noticing, and a broken build is declined rather than adopted.

The reload is Unix-specific: it rests on `execve` preserving descriptors and on
a rename-over leaving the old inode intact. That is the platform this runs on.

Deliberately not decided: an HTTP or SSE transport. Nothing needs it yet, and
the stdio server is the one a local harness spawns. The reload design does not
transfer to a long-lived socket server holding client state, and that would be a
different decision rather than an extension of this one.

Not offered: read-only mode as a server flag. `readOnly` travels with every tool
so a harness can withhold mutation where it configures its tools, which is where
that policy belongs. A flag here would be a second place to express it, and the
two would disagree.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) §6 — narrow operations, not arbitrary write SQL
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — the manifest the tool list is built from
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the refusal rules the adapter inherits
