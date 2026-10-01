# ADR-058: Plugins are pinned dispatcher actions, called synchronously and rechecked on every call

**Status:** Proposed
**Date:** 2026-10-01
**Deciders:** George
**Sources:** kb kanban `a-843bb89d` (PLUGIN slice admitted, 2026-09-30), `a-9056d954` and
`a-5f631e1d` (owner verdicts, 2026-10-01); George's 2026-09-21 decisions on `t-f73a81ae` and epic
`e-d11a4650` (private `kb-plugin-ix`, configuration-loaded, never a submodule, public Kanban works
with the plugin absent).

## Context

George's personal installation needs a private planning plugin (`kb-plugin-ix`) that composes the
IFCA-wide ix-bot with the Kanban ledger. Public Kanban may carry none of that code, no private
dependency and no credential. It must still run with no plugin at all.

Kanban already runs operator-configured executables: the subscription dispatcher reads
`dispatchers.json` (consumers, capabilities, actions, secret references), checks file and
executable permissions, clears the environment down to one configured secret, and runs the
adapter in its own process group with timeouts and bounded streams (ADR-031,
`rust/dispatch.rs`, `rust/adapter_process.rs`). That path is asynchronous and at-least-once: a
delivery is a notification, not an answer. A planning plugin is asked a question and must answer
it now. Nothing in the code is named plugin, nothing records which build of an executable ran,
and no synchronous call path exists.

Work is specified in `docs/specs/plugin.md` (slice `PLUGIN`, `PLUGIN-01` .. `PLUGIN-15`).

## Decision

1. **One trust root: `dispatchers.json` version 2** (`PLUGIN-01` .. `PLUGIN-04`; owner verdict
   `a-9056d954`). A consumer action may be `"kind": "plugin"`. A plugin action pins the build two
   ways — `sha256` of its entry executable and `revision`, the build identity the plugin must
   report — and carries `timeoutMs` and an optional `secret` id. Version 1 files load unchanged.
   An older binary refuses version 2 rather than half-reading it. A missing file means "no
   plugins", never an error, so a public install runs exactly as before.
2. **Read-only capability** (`PLUGIN-02`). The only plugin capability is `plugin.read`. Mutating
   plugins need a later slice (`t-6403e808`); reads and mutations stay separate (epic
   `e-d11a4650`).
3. **A synchronous entry point that never meets the delivery path** (`PLUGIN-05`, `PLUGIN-06`).
   `kanban plugin call CONSUMER ACTION` runs the plugin once, waits, and writes nothing.
   Subscriptions refuse plugin actions and `plugin call` refuses delivery actions, so an async
   delivery can never stand in for a planning or claim answer (owner verdict `a-9056d954`).
4. **Recheck everything on every call** (`PLUGIN-07`, `PLUGIN-09`; owner verdict `a-5f631e1d`).
   Each call reloads the file and re-verifies consumer, action, capability, secret source,
   executable rules and the executable hash before spawning, then checks the reported revision
   after the run. Nothing is cached between calls.
5. **Exact protocol, canonical output** (`PLUGIN-08`, `PLUGIN-10`, `PLUGIN-11`). Plugin protocol
   v1 reuses the adapter envelope rules (1 MiB, camelCase, unknown fields refused, no trailing
   bytes) and the adapter process rules. Kanban prints one canonical envelope (keys sorted, no
   whitespace, `schemaVersion` 1, consumer, action, revision, sha256, output), so equal plugin
   output gives byte-identical lines.
6. **One command table** (`PLUGIN-12`, `PLUGIN-13`). `plugin list` shows each plugin action and why
   it is or is not callable. Both verbs become the MCP tools `plugin_call` and `plugin_list`
   through the existing generated manifest (ADR-010, ADR-011).
7. **The public tree stays plugin-free** (`PLUGIN-14`, `PLUGIN-15`). `Cargo.lock` holds only
   crates.io packages and this package; no plugin config ships; secrets stay references and
   their values never appear in Kanban's own output.

## Alternatives rejected

- **A separate operator-owned `plugins.json`.** Rejected by George on `a-9056d954`: a second
  trust root duplicates the executable and secret checks and can drift from them.
- **Validate only at load.** Rejected by George on `a-5f631e1d`: a grant revoked or a binary
  replaced after load would stay callable until restart.
- **Pin only the executable hash.** An interpreted plugin's code sits beside its entry file, so
  the hash alone would miss a changed build. The reported revision covers that, and the hash
  still stops a replaced entry file before it runs.
- **Pin only a reported revision.** The plugin would have already run before the mismatch was
  seen. The hash refuses a replaced executable before it runs.
- **Per-plugin MCP tools from a plugin manifest.** This would make the tool list depend on runtime
  configuration and break ADR-010's single description. The generic `plugin_call` reaches every
  action.
- **A Git submodule or a Cargo dependency on the plugin.** Rejected by George on 2026-09-21: the
  plugin is private and must never enter public source, lockfiles or builds.
- **Reusing the subscription dispatcher for calls.** Its at-least-once, lease-and-retry delivery
  is the wrong contract for a question that needs an answer now, and owner verdict `a-9056d954`
  forbids treating a delivery as a plan or claim decision.

## Consequences

- `dispatchers.json` gains a version. Operators opt in by writing version 2, and a version 2 file
  pins every plugin build, so `plugin list` is a record of what is installed.
- Each call reloads one small file and hashes one executable. George accepted "more checks" for
  "no stale grant window" (`a-5f631e1d`).
- A plugin still runs with the operator's uid. The executable rules limit replacement between hash
  and spawn to that uid; this ADR adds no sandbox.
- Implementation is row `t-f73a81ae`, which writes the tests named in the slice's trace.
