# Using Kanban as an MCP server

`kanban mcp` speaks MCP over stdio. It exposes one tool per CLI operation and
runs the real command for each call, so an agent gets the same validation and
the same refusals an operator does.

## Wiring it into a client

The server is spawned by the client and talks over stdin/stdout. It takes no
arguments and no configuration beyond the environment every Kanban command
already reads.

```json
{
  "mcpServers": {
    "kanban": {
      "command": "kanban",
      "args": ["mcp"],
      "env": { "KANBAN_PROJECT": "unum" }
    }
  }
}
```

`KANBAN_PROJECT` is optional. Without it the server resolves a board the way any
command does — from the working directory it was spawned in — and every tool
still accepts `project`, `workspace` and `db` per call. Exactly one of those may
be given at a time; two that disagree are refused rather than ranked
([ADR-007](adr/ADR-007-global-project-addressing.md)).

## What the tools are

One per operation in `kanban schema --json`, named with spaces and dashes
flattened: `task add` becomes `task_add`, `import atmux-sqlite` becomes
`import_atmux_sqlite`. Positionals are named arguments (`task_move` takes `id`
and `status`), flags keep their own names, and a repeatable flag is typed as an
array so an agent can pass more than one value.

Every tool carries `annotations.readOnlyHint`. It is true only for operations
that write nothing anywhere — not the board, not the registry, not a file — so
`backup` and `todo` are not read-only despite changing no work state. A harness
that withholds mutation should gate on that hint; it is checked by a test that
runs each read-only operation and requires the board file to be byte-identical
afterwards.

A command that refuses comes back as a tool result with `isError: true` carrying
the command's own message. Those messages name the fix, so pass them to the
model rather than replacing them.

## Updating without restarting

`install` over the binary. Nothing else is needed, and no client has to
reconnect:

```bash
cargo build --release --locked
install -m 0755 target/release/{kanban,kb} /root/.local/bin/
```

Running servers pick the new build up on their own. Tool calls spawn the binary
from disk, so **operations run the new code from the first call after the
install**. The server process itself replaces its own image between requests —
same process id, same pipes — so the protocol layer and tool list follow within
one request. A build that does not run is declined, and the previous one keeps
serving with a note on stderr.

The reasoning, and the three conditions the swap waits for, are in
[ADR-011](adr/ADR-011-in-binary-mcp-server-and-in-place-reload.md).

## What it does not do

- No HTTP or SSE transport. Stdio is what a local harness spawns.
- No read-only server mode. `readOnlyHint` travels with each tool, so that
  policy belongs where the harness configures its tools; a server flag would be
  a second place to say it and the two would disagree.
- No validation of its own. The manifest describes the surface, not the
  semantics: it says `task_move` takes a status, not which statuses are legal or
  that a story's status is projected from its gate. The CLI answers that, once.
