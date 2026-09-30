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

**Amended 2026-09-03 by
[ADR-033](ADR-033-principals-are-frozen-username-plus-uid-and-minted-through-a-peer-credential-broker.md):**
the client-facing transport remains stdio and this generated CLI hop remains a
real process boundary. Under managed multi-user enforcement, however, neither
the MCP process nor the spawned command opens registry, board, backup, or index
files. The spawned command makes a separate short-lived Unix-domain-socket
connection to the access broker. The broker derives that connection's peer UID
with `SO_PEERCRED`, authorizes the operation, opens the stores, and executes it.
Stdin, the MCP parent, JSON-RPC fields, tool arguments, `--as`, environment,
and board selectors do not carry authority or bypass the broker.

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
one request later. That lag is real and is not worth removing. Tool calls spawn
the command binary from disk, so the first call after an install runs the new
**command-side** code. Under ADR-033 that is not an end-to-end freshness claim:
the separately running broker still executes authorization and data access.
Every command first negotiates the exact broker protocol version, generated
command-schema hash, policy-schema version, supported board-schema range, and
both binary identities. ADR-033 fixes the first broker protocol at integer `1`,
requires exact protocol and command-schema equality, and requires overlapping
schema support for the live registry and every target board. An incompatible
or unavailable broker refuses before opening a board. A compatible but older
broker is named as older in the result and audit receipt; it is never described
as fresh merely because the command binary changed.

The server does not require `initialize` before serving. Insisting on it would
make a reload visible: the new image did not witness the handshake, and would
reject a client that is, correctly, not going to repeat it.

## Consequences

`kanban mcp` is a complete MCP server with no new dependencies. The transport is
newline-delimited JSON-RPC over stdio, which needs nothing beyond `serde_json`
and the standard library, and the tool list is the ADR-010 manifest.

ADR-033 adds a broker socket as an internal authorization hop, not as another
MCP transport. The client still spawns `kanban mcp`, the MCP server still owns
only the stdio protocol state, and each tool request's command process closes
its broker connection before returning. The broker is a separately owned local
service; it is not the long-lived socket server rejected below as an MCP
transport. Broker protocol and schema negotiation occurs on every such
connection, before an operation is accepted.

An MCP/CLI update remains `install` over that binary. Running stdio servers pick
up the MCP protocol layer and generated tool surface without a client
reconnect, and a broken command build is declined rather than adopted. That
install does not update the broker. Broker replacement is a separate managed
service operation: preflight replay and compatibility, stop accepting, drain
accepted requests, restart under the broker owner, negotiate the new identity,
then resume clients. Failure stays fail-closed; it never makes command
processes open managed stores directly. An unaccepted request may reconnect,
but an accepted request is not replayed unless that operation carries its own
idempotency key.

The reload is Unix-specific: it rests on `execve` preserving descriptors and on
a rename-over leaving the old inode intact. That is the platform this runs on.

**Decided 2026-08-21: stdio only, no HTTP or SSE transport.** Every consumer
this exists for — Claude Code, Codex, Kimi, orch, atmux — spawns a local
subprocess, which is what stdio is. A socket transport would add a listener, a
port, an authentication story and a lifetime nobody currently owns, to reach
clients that do not exist.

It would also cost the reload. The whole in-place swap rests on the server
holding nothing between requests and on `execve` inheriting the client's pipe. A
long-lived socket server holds accepted connections and per-client protocol
state, so replacing its image mid-flight drops them; that is a different design
with a different mechanism (drain, hand off the listening descriptor, or accept
a restart), not an extension of this one. Adding the transport would therefore
quietly downgrade the property that made this worth building.

If a remote consumer ever appears, the honest shape is a separate front end that
speaks the network protocol and drives this binary, keeping the local server —
and its reload — exactly as it is.

Not offered: read-only mode as a server flag. `readOnly` travels with every tool
so a harness can withhold mutation where it configures its tools, which is where
that policy belongs. A flag here would be a second place to express it, and the
two would disagree.

## Amendment — 2026-08-21: what the adapter must not forward

Auditing the surface immediately after it shipped found three ways a tool call
could report success without doing what was asked. All three had one shape: a
value this layer accepted and quietly reinterpreted.

**`--help` and `--json` were advertised as absent and accepted anyway.** They
were filtered out of the generated schema and left in the set the argument check
allowed, because those were two lists rather than one. `help: true` answered a
`task_list` call with the usage page and reported `isError: false` — the
operation never ran, and nothing said so. `json: true` produced `--json given
more than once`, since this layer appends it to every call: a refusal blaming
the caller for a flag they passed once. One list now feeds the schema and the
check, so a flag cannot be advertised and honoured differently.

**A composite value was flattened into a single argument.** `title: ["a", "b"]`
recorded the title `["a","b"]` and returned success. Stringifying a caller's
mistake into durable state is precisely what this ledger exists to prevent, and
an array where one value belongs is not a value. Scalars still convert — a JSON
number reaching `--priority` is not a type error — but an array or an object
where one value is expected is refused, and an array is accepted only for the
flags declared repeatable.

**`arguments` that was not an object was read as no arguments at all.** A call
whose parameters were malformed ran unconstrained and reported success, which is
the worst of the three: the caller asked for something narrow and got everything.

The common lesson is the one ADR-008 already states, arriving through a new
surface: an adapter that accepts more than it advertises has silently taken on a
second, undocumented contract, and every difference between the two is a defect
waiting to be reported as success.

## References

- [ADR-001](ADR-001-durable-agent-work-ledger.md) §6 — narrow operations, not arbitrary write SQL
- [ADR-010](ADR-010-adapters-generated-from-the-command-surface.md) — the manifest the tool list is built from
- [ADR-008](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md) — the refusal rules the adapter inherits
- [ADR-033](ADR-033-principals-are-frozen-username-plus-uid-and-minted-through-a-peer-credential-broker.md) — brokered principal and policy boundary without changing client-facing stdio
