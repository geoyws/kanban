# Specification: an operator-configured plugin is called synchronously through the existing dispatcher trust root, pinned and rechecked on every call, and a host without plugins runs unchanged (slice PLUGIN)

## 1. Identity and baseline

- **Slice ID:** `PLUGIN`. Requirement IDs are `PLUGIN-01` .. `PLUGIN-15`, stable across wording
  refinements; numbering is by creation, grouping is by topic.
- **Baseline:** `2026-10-01` at commit `46964bc` (`origin/kanban-geoyws-driver`) in
  `/Users/geoyws/work/wt/kanban-t-374806ea-plugin-w1-bb7338`, branch `wt/t-374806ea-plugin`.
  Every "today" claim below cites `<path>:<line>` in that worktree. At the baseline no type, field,
  module or verb named plugin exists in `rust/`; the only mentions are prose (`rust/lib.rs:2719-2720`,
  ADR-001 §6, ADR-010, ADR-031 §8).
- **Status:** `SPEC-READY` on 2026-10-01 (independent reviewer agent `ReviewPluginSpec1` at commit
  `79d3408`, applying the SDD §1 exit criteria: fifteen MUST requirements, each testable and
  reached by an acceptance example and one §8 row; both material owner choices taken on
  `a-9056d954` and `a-5f631e1d`; cited lines spot-checked in this worktree. Its three wording
  findings are applied as recorded in §9. Specification readiness only — it authorises neither
  implementation, nor rollout, nor release.)
- **Owner (product scope):** George. He alone resolves scope and the open questions in §7.
- **Decider (wording of this document):** slice row `t-374806ea` under epic `e-c0852fe7`,
  admitted on owner verdict `a-843bb89d` (kind `approval`, resolved by geoyws 2026-09-30:
  "PLUGIN slice approved; docs/specs/plugin.md plus ADR, independent review, stop at
  SPEC-READY").
- **Sources:**
  - Owner verdict `a-9056d954` (2026-10-01): "Reuse existing dispatcher binding. One trust root and
    existing executable/secret grants; implementation must add the plugin-specific synchronous
    entry point without treating an async delivery as a plan/claim decision."
  - Owner verdict `a-5f631e1d` (2026-10-01): "Startup shape plus each invocation. Validate
    configured shape at load and recheck capability, availability and pinned revision before every
    plugin-bound operation; more checks but no stale grant window." Note: "pluginless hosts run
    normally."
  - George, 2026-09-21, on `t-f73a81ae` and epic `e-d11a4650`: the plugin is a separate private
    repository loaded "through explicit operator configuration, NOT Git submodules"; "Public
    Kanban must have only provider-neutral plugin and coordination interfaces: no ix-bot/plugin
    source, private-repo gitlink, hardcoded private dependency, private credentials or accidental
    private-code inclusion in public source/build/release artifacts. Public clone/build/test/run
    works with this optional plugin absent; configured-but-unavailable or unauthorized plugin
    fails clear". Epic `e-d11a4650`: "install an immutable private plugin revision and record
    it"; "Reads and mutations remain separate capabilities"; identical inputs "produce
    byte-identical canonical JSON".
  - Implementing row `t-f73a81ae` (blocked on this specification): canonical JSON, bounded
    fields and errors, outputs carrying the provider revision and a schema version.
  - `docs/adr/ADR-058-plugins-are-pinned-dispatcher-actions-called-synchronously.md` — the
    decision this slice implements.
  - `docs/adr/ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md` — every refusal
    names what was refused and writes nothing.
  - `docs/adr/ADR-010-adapters-generated-from-the-command-surface.md` and
    `docs/adr/ADR-011-in-binary-mcp-server-and-in-place-reload.md` — CLI, schema manifest and MCP
    tools come from one command table.
  - Shipped surface at the baseline:
    - `dispatchers.json` is loaded from the data root (`rust/dispatch.rs:15`, `:220-221`,
      `:491-495`; root resolution `rust/registry.rs:466-471`), at `version` 1 only
      (`rust/dispatch.rs:26`, `:497`), with `deny_unknown_fields` at every level
      (`rust/dispatch.rs:29-57`): `consumers.<id>.capabilities`, `actions.<id>.{capability,
      executable, args}`, `secrets.<id>.{sourceEnv, targetEnv}`.
    - Load checks: data root mode `& 0o077 == 0` (`rust/dispatch.rs:224-233`); config file not a
      symlink, regular, private, unchanged while opening (`rust/dispatch.rs:283-301`); at most
      1 MiB (`rust/dispatch.rs:16`, `:323`); at least one consumer (`:500`) and one action per
      consumer (`:506`); capabilities declared once (`:333-338`); executable absolute, regular,
      non-symlink, executable, not group/other-writable (`:346-378`).
    - Resolve-time checks: unknown consumer, unknown action, undeclared capability, unknown
      secret, missing source env (`rust/dispatch.rs:527-562`).
    - Process rules: absolute executable, timeout `1..=300000` ms (`rust/adapter_process.rs:233-238`),
      `env_clear()` plus only the configured secret (`:244-258`), own process group (`:249`), 1 MiB
      per stream (`:11`), SIGTERM then SIGKILL on timeout or cancel (`:305-368`), named failure
      classes (`:370-374`).
    - The adapter envelope is v1, 1 MiB, `deny_unknown_fields`, no trailing bytes
      (`rust/adapter_protocol.rs:5`, `:9-44`, `:100-123`).
    - The only production caller of `run_process` is the async subscription dispatcher
      (`rust/dispatcher.rs:709-740`); it reloads `dispatchers.json` for every candidate
      (`rust/dispatcher.rs:680-681`). No synchronous call path exists.
    - MCP tools are generated from the command table (`rust/mcp.rs:243-323`), carry `readOnlyHint`,
      report a failed command as a result with `isError: true` (`rust/mcp.rs:401-450`), and skip
      long-running commands (`rust/lib.rs:541`).
- **Trace matrix:** `docs/testing/compiled-rust-e2e-matrix.md`, section
  `## Requirements trace — docs/specs/plugin.md PLUGIN-01..PLUGIN-15`. §8 is its draft.

## 2. Purpose and scope

**Intended outcome.** An operator names a private plugin in the existing `dispatchers.json`, pins
the exact build, and any lane can call it synchronously from the CLI or MCP. Kanban rechecks the
grant and the pin on every call, returns canonical bytes, and refuses clearly when anything is
off, while a public install with no plugin behaves exactly as today.

**Users / actors.**

- The operator (George), who installs a plugin build on one host and records it in
  `dispatchers.json` with its pins and secret references.
- Agent lanes and MCP clients, which call a plugin action and read its canonical output.
- The plugin author (for example the private `kb-plugin-ix`), who implements the plugin protocol
  in §5 and nothing Kanban-specific beyond it.

**In scope.** `dispatchers.json` version 2 (plugin actions, their pins and timeout); the
`plugin.read` capability; the verbs `plugin call` and `plugin list`; the plugin request/response
protocol v1; the canonical output envelope; the separation between plugin actions and
subscription delivery; the matching MCP tools; the public-tree invariant.

**Boundaries.** The subscription dispatcher, its delivery envelope and its at-least-once rules
(ADR-031) are unchanged except that they refuse plugin actions (PLUGIN-05). Plugin code, its
repository, its build and its own determinism belong to the plugin author. Secret values stay in
the operator's environment; only references live in the file. The broker's read/write/admin
policy lattice (`rust/policy.rs`) is not extended.

**Non-goals.**

- Mutating plugins. Reads and mutations are separate capabilities (epic `e-d11a4650`); this slice
  admits `plugin.read` only. A write capability is a later slice (`t-6403e808`).
- A second configuration file, a plugin directory scan, or auto-discovery. Owner verdict
  `a-9056d954` keeps one trust root.
- Per-plugin MCP tools generated from a plugin's own manifest. The tool list stays a function of
  the command table (ADR-010); plugin actions are reached through `plugin_call`.
- Giving a plugin access to the board. A plugin receives only the caller's input and its own
  configured secret; Kanban passes no board path, no actor and no lease.
- Sandboxing. A plugin runs with the operator's uid in its own process group, as adapters do
  today (`rust/adapter_process.rs:22-25`, `:249`); containment is not isolation.

## 3. Requirements

Strength keywords are BCP 14. `Layer` names the one layer that proves the requirement:

- `unit` — an in-process Rust `#[test]` reading produced bytes.
- `process` — a compiled-binary process-boundary exchange, no HTTP and no browser.

This slice changes no served markup, so no requirement uses `http` or `chrome`.

### Configuration

**PLUGIN-01** — `dispatchers.json` version 2 admits plugin actions.
Strength: `MUST` · Layer: `process` · Source: `a-9056d954`; `rust/dispatch.rs:26`, `:29-57`.
`The loader accepts "version": 1 exactly as today and "version": 2. In version 2 an action may
carry "kind": "plugin" ("kind" absent or "delivery" keeps today's meaning). A plugin action
requires "revision" (1-128 ASCII characters, starting with a letter or digit, then letters,
digits, dot, underscore or hyphen), "sha256" (exactly 64 lowercase hexadecimal characters) and
"timeoutMs" (an integer from 1 to 300000 inclusive, matching rust/adapter_process.rs:233-238), and
may carry "secret" (the id of one entry in its
consumer's "secrets" map). A delivery action takes its secret from the subscription row's
secret reference (rust/dispatch.rs:553-562); a plugin call has no subscription, so the action
names its own. "kind", "revision", "sha256", "timeoutMs" and "secret" are refused in a version 1
file, and "revision", "sha256", "timeoutMs" and "secret" are refused on a delivery action. A
"secret" naming no entry of its consumer is refused at load. Every other key stays
deny_unknown_fields.`
`Failure behaviour: a violating file is refused at load with a sentence naming the consumer, the
action and the field, and no plugin call or delivery runs from it.`
`Data rules: the file is read, never written, by Kanban.`

**PLUGIN-02** — A plugin action's capability is `plugin.read`.
Strength: `MUST` · Layer: `process` · Source: epic `e-d11a4650` ("Reads and mutations remain
separate capabilities").
`A plugin action names capability "plugin.read", and its consumer declares "plugin.read" among
its capabilities. Any other capability on a plugin action is refused at load with "plugin action
<consumer>/<action> must use capability plugin.read".`

**PLUGIN-03** — Plugin shape is validated when the file loads.
Strength: `MUST` · Layer: `process` · Source: `a-5f631e1d` ("validate configured shape at load").
`Every load of dispatchers.json — by plugin list, by plugin call, and by the dispatcher — applies
PLUGIN-01 and PLUGIN-02 together with today's load checks (rust/dispatch.rs:224-233, :283-301,
:323-378, :497-506) before any process is spawned. The first violation is reported; nothing is
spawned and nothing is written.`

**PLUGIN-04** — A host without plugins runs normally.
Strength: `MUST` · Layer: `process` · Source: `a-5f631e1d` note ("pluginless hosts run
normally"); George 2026-09-21 ("Public clone/build/test/run works with this optional plugin
absent").
`When dispatchers.json is absent, plugin list prints an empty list ("[]" under --json) and exits
0. When it is present but holds no plugin action, the same. Every command other than plugin call
behaves byte-for-byte as at the baseline. plugin call on such a host is refused with
"no plugin <consumer>/<action> is configured".`
`Failure behaviour: a present but invalid file still fails closed (PLUGIN-03); absence is the only
state treated as "no plugins".`

**PLUGIN-05** — Plugin actions and subscription delivery never cross.
Strength: `MUST` · Layer: `process` · Source: `a-9056d954` ("without treating an async delivery
as a plan/claim decision").
`subscription add naming a consumer/action that resolves to a plugin action is refused with
"action <consumer>/<action> is a plugin; subscriptions deliver only to delivery actions". The
dispatcher re-checks at resolve time (rust/dispatcher.rs:680-681) and fails a candidate whose
action became a plugin with the same sentence, without spawning it. plugin call naming a delivery
action is refused with "action <consumer>/<action> is not a plugin action".`

### Invocation

**PLUGIN-06** — `plugin call` runs one plugin action synchronously and writes nothing.
Strength: `MUST` · Layer: `process` · Source: `a-9056d954` ("plugin-specific synchronous entry
point").
`kanban plugin call CONSUMER ACTION [--input-json JSON | --input-file PATH] [--json] runs the
named plugin action once, waits for it, and prints its result (PLUGIN-11). Input defaults to {}
and MUST be a JSON object; anything else is refused before any check in PLUGIN-07 runs. The call
writes no board row, no registry row and no event, on success or failure.`
`Permissions: any caller who can run the binary; the grant is the operator's dispatchers.json
(PLUGIN-07), not the board's access policy.`

**PLUGIN-07** — Every call rechecks grant, availability and pin before it spawns.
Strength: `MUST` · Layer: `process` · Source: `a-5f631e1d` ("recheck capability, availability and
pinned revision before every plugin-bound operation").
`Before spawning, each plugin call (and each plugin list availability probe, PLUGIN-12) reloads
dispatchers.json from disk and, in this order: resolves the consumer and the action; confirms the
action is a plugin (PLUGIN-05); confirms the consumer declares the action's capability; resolves
the action's "secret", if any, and its source environment variable; verifies the executable is
absolute, regular, not a symlink, executable and not group- or other-writable
(rust/dispatch.rs:367-378); and computes the SHA-256 of the executable's bytes and compares it
with "sha256". The first failure refuses the call with one sentence naming the consumer, the
action and the reason — for the pin, "plugin <consumer>/<action> is unavailable: executable
sha256 <found> does not match pinned <pinned>" — and nothing is spawned.`
`Data rules: no result of an earlier call or an earlier load is cached between calls.`

**PLUGIN-08** — The plugin protocol is versioned, bounded and exact.
Strength: `MUST` · Layer: `unit` · Source: `t-f73a81ae` ("bounded fields and errors");
`rust/adapter_protocol.rs:5`, `:9-44`, `:100-123`.
`Kanban writes one request to the plugin's stdin and reads one response from its stdout, both
JSON objects of at most 1 MiB, nothing but whitespace after the object. The envelope fields are
exactly those below, named in camelCase; an envelope with a missing or unknown field is refused.
The "input" and "output" objects are opaque to these rules: any keys, in any case, at any
depth. Request: {"protocolVersion":1,"target":{"consumerID":..,
"actionID":..},"revision":<pinned revision>,"input":<object>}. Response:
{"protocolVersion":1,"revision":<string>,"output":<object>}. A response that breaks any rule is
refused with "plugin <consumer>/<action> returned an invalid response: <rule>", and its output is
discarded.`

**PLUGIN-09** — The plugin must report the pinned revision.
Strength: `MUST` · Layer: `process` · Source: epic `e-d11a4650` ("install an immutable private
plugin revision and record it").
`A response whose "revision" differs from the action's pinned "revision" is refused with
"plugin <consumer>/<action> reported revision <reported>, pinned <pinned>", and its output is
discarded. The executable hash (PLUGIN-07) pins the entry file before the run; the reported
revision pins the build the plugin says it is, which covers an interpreted plugin whose code sits
beside its entry file.`

**PLUGIN-10** — The plugin runs under today's adapter process rules.
Strength: `MUST` · Layer: `process` · Source: `rust/adapter_process.rs:227-374`.
`The plugin is spawned with its configured absolute executable and args, an environment cleared
of everything except its configured secret target variables, its own process group, 1 MiB per
output stream, and the action's timeoutMs. A non-zero exit, a timeout, a stream overflow or a
spawn failure refuses the call with "plugin <consumer>/<action> failed: <class>", where <class>
is the failure class the process runner reports (for example adapter_timeout, adapter_exit,
adapter_stdout_overflow). stderr is never printed on stdout; secret values never appear in any
refusal.`

**PLUGIN-11** — The call prints one canonical envelope.
Strength: `MUST` · Layer: `process` · Source: epic `e-d11a4650` ("byte-identical canonical
JSON"); `t-f73a81ae` ("outputs carry provider revision ... schema version").
`On success plugin call prints exactly one line, with or without --json:
{"action":..,"consumer":..,"output":<the plugin's output object>,"revision":..,"schemaVersion":1,
"sha256":..} in canonical form — object keys sorted by their UTF-8 bytes at every depth, no
insignificant whitespace, numbers re-serialized from their parsed value (an integer that fits in
64 bits in plain decimal; any other number as the shortest decimal that parses back to the same
IEEE-754 double, so 1.0 and 1E0 both print 1.0), one trailing newline — and exits 0. Two calls
whose plugin outputs are equal as JSON values print byte-identical lines.`
`Failure behaviour: with --json a refusal prints {"error":"<sentence>"} on stdout and the sentence
on stderr, exits non-zero, and prints no partial output (the refusal shape at
tests/e2e.rs refusal_object).`

### Listing

**PLUGIN-12** — `plugin list` shows each plugin action and whether it is callable now.
Strength: `MUST` · Layer: `process` · Source: George 2026-09-21 ("configured-but-unavailable or
unauthorized plugin fails clear").
`kanban plugin list [--json] prints one row per plugin action, sorted by consumer then action,
with consumer, action, capability, revision, sha256, executable, available (true or false) and
reason (null when available, otherwise the PLUGIN-07 sentence). It runs the PLUGIN-07 checks for
each row without spawning anything. It never prints args, secret values, secret source variable
names or environment values. A load failure (PLUGIN-03) refuses the whole listing.`

### Surfaces

**PLUGIN-13** — Both verbs are MCP tools from the same command table.
Strength: `MUST` · Layer: `process` · Source: ADR-010, ADR-011; `rust/mcp.rs:243-323`,
`:401-450`.
`The generated MCP manifest carries plugin_call and plugin_list with readOnlyHint true, their
flags and positionals taken from the command table like every other tool. A plugin_call that the
CLI would refuse returns a tool result with isError true carrying the same sentence; a successful
one returns the same canonical line PLUGIN-11 prints.`

### Privacy

**PLUGIN-14** — The public tree carries no plugin and no private dependency.
Strength: `MUST` · Layer: `unit` · Source: George 2026-09-21 ("no ix-bot/plugin source,
private-repo gitlink, hardcoded private dependency").
`Every package in Cargo.lock comes from the crates.io registry or is this repository's own
package; no git, path or alternate-registry source appears. The repository ships no
dispatchers.json and no default plugin entry, and .gitmodules names no plugin repository.`

**PLUGIN-15** — Secrets stay references.
Strength: `MUST` · Layer: `process` · Source: epic `e-d11a4650` ("credentials stay in encrypted
store with pointer-only configuration").
`A plugin action reaches a secret only through its "secret" field naming a secrets.<id> entry of
its consumer (PLUGIN-01; the entry shape is rust/dispatch.rs:52-57), and at most one secret, as a
delivery does (rust/adapter_process.rs:29). No value read from a secret's source variable
appears in plugin list output, in the PLUGIN-11 envelope (other than inside the plugin's own
output object), or in any refusal sentence.`

## 4. Acceptance examples

Concurrency needs no example of its own: plugin call writes nothing (PLUGIN-06) and caches nothing
(PLUGIN-07), so two concurrent calls share no state Kanban owns. Idempotency is PLUGIN-11's
byte-identity. Unauthorised access is A4.

### A1 (PLUGIN-04, PLUGIN-14)

*Given* a fresh data root with no `dispatchers.json`,
*when* the caller runs `plugin list --json`, then `task add`, `watch --cursor 0 --json`, and
`plugin call acme lookup --json`,
*then* the listing prints `[]` and exits 0, the two board commands behave as at the baseline, and
the call is refused with `no plugin acme/lookup is configured`; and the repository's `Cargo.lock`
lists only crates.io sources and this package.

### A2 (PLUGIN-01, PLUGIN-02, PLUGIN-03)

*Given* a version 1 file whose action carries `"kind": "plugin"`, and separately a version 2 file
whose plugin action has `"sha256": "ABC"`, and a third whose plugin action names capability
`deliver`,
*when* the caller runs `plugin list --json` against each,
*then* each is refused naming the field (`kind`, `sha256`) or the capability rule, and nothing is
spawned.

### A3 (PLUGIN-06, PLUGIN-07, PLUGIN-08, PLUGIN-09, PLUGIN-10, PLUGIN-11)

*Given* a version 2 file with consumer `acme` declaring `plugin.read`, plugin action `lookup`
pointing at an executable fixture that echoes `{"protocolVersion":1,"revision":"r1",
"output":{"b":2,"a":[1,{"d":4,"c":3}]}}`, pinned with `revision` `r1` and the fixture's SHA-256,
*when* the caller runs `plugin call acme lookup --input-json '{"q":1}' --json` twice,
*then* both runs print the same single line
`{"action":"lookup","consumer":"acme","output":{"a":[1,{"c":3,"d":4}],"b":2},"revision":"r1","schemaVersion":1,"sha256":"<hash>"}`,
the fixture saw a request with `protocolVersion` 1, `revision` `r1` and input `{"q":1}`, and the
board's ledger head did not move.

### A4 (PLUGIN-07, PLUGIN-12, PLUGIN-15)

*Given* the A3 setup,
*when* one byte of the fixture executable changes, *and separately* the consumer stops declaring
`plugin.read`, *and separately* the action's secret source variable is unset,
*then* each `plugin call` is refused with its PLUGIN-07 sentence before the fixture runs (a marker
file the fixture writes on start is absent), `plugin list --json` shows that row with
`"available": false` and the same sentence as `reason`, and no output line contains the secret's
value or its source variable name.

### A5 (PLUGIN-08, PLUGIN-09, PLUGIN-10)

*Given* the A3 setup with the fixture switched to report revision `r2`, then to print a trailing
token after its object, then to exit 3, then to sleep past `timeoutMs`,
*when* the caller runs `plugin call acme lookup --json` for each,
*then* each is refused with the matching sentence (`reported revision r2, pinned r1`, `invalid
response`, `failed: adapter_exit`, `failed: adapter_timeout`), stdout holds only the
`{"error":..}` object, and none prints the plugin's output.

### A6 (PLUGIN-05)

*Given* the A3 setup plus a delivery action `notify` on the same consumer,
*when* the caller runs `subscription add` naming `acme`/`lookup`, then `plugin call acme notify`,
*then* the first is refused with `is a plugin; subscriptions deliver only to delivery actions` and
writes no subscription row, and the second with `is not a plugin action`.

### A7 (PLUGIN-13)

*Given* the A3 setup,
*when* an MCP client lists tools and calls `plugin_call` with `consumer` `acme`, `action`
`lookup` and the A3 input, then with action `nope`,
*then* both tools are listed with `readOnlyHint` true, the first result's text is the A3 line with
`isError` false, and the second has `isError` true and the CLI's refusal sentence.

## 5. Contracts and data

- **Interface version or schema:** `dispatchers.json` versions 1 and 2 (§3 PLUGIN-01). CLI grammar:
  `kanban plugin call CONSUMER ACTION [--input-json JSON | --input-file PATH] [--json]` and
  `kanban plugin list [--json]`, both read-only. Plugin protocol v1 (PLUGIN-08). Output envelope
  `schemaVersion` 1 (PLUGIN-11). MCP tools `plugin_call` and `plugin_list` (PLUGIN-13).

  Example version 2 file:

  ```json
  {
    "version": 2,
    "consumers": {
      "acme": {
        "capabilities": ["plugin.read"],
        "secrets": {"acme-token": {"sourceEnv": "ACME_TOKEN", "targetEnv": "ACME_TOKEN"}},
        "actions": {
          "lookup": {
            "kind": "plugin",
            "capability": "plugin.read",
            "executable": "/opt/acme-plugin/bin/acme-plugin",
            "args": ["serve-once"],
            "secret": "acme-token",
            "revision": "3f1c2ab",
            "sha256": "<64 lowercase hex>",
            "timeoutMs": 30000
          }
        }
      }
    }
  }
  ```

  The secret entry is the consumer's existing `secrets` map; the plugin action selects one entry
  by id, because there is no subscription row to carry the reference.
- **Data invariants:** plugin call and plugin list write nothing to any board, registry or ledger;
  Kanban never writes `dispatchers.json`; a plugin action always carries both pins and a timeout.
- **Migration:** none for boards or the registry. `dispatchers.json` version 1 files keep loading
  unchanged; an operator opts in by writing version 2.
- **Compatibility:** an older binary refuses a version 2 file (`unsupported dispatcher config
  version 2`, `rust/dispatch.rs:497`), so a host never runs a plugin configuration it cannot
  check. A version 2 file without plugin actions is accepted by the dispatcher exactly as a
  version 1 file.
- **Ownership:** the operator owns `dispatchers.json` and the installed plugin build; the plugin
  author owns the plugin's output and its determinism; Kanban owns the checks, the envelope and
  the canonical serialization.

## 6. Quality and security

- **Reliability:** N/A beyond PLUGIN-10 — a call is one synchronous process with no retry and no
  queue; the caller decides whether to call again.
- **Accessibility:** N/A — no served markup.
- **Privacy:** PLUGIN-14 keeps private code and dependencies out of the public tree; PLUGIN-15
  keeps secret values out of every Kanban-produced byte; the plugin gets no board access (§2).
- **Security:** fail-closed configuration (PLUGIN-01..03), per-call recheck with no cached grant
  (PLUGIN-07), two pins (PLUGIN-07 hash before the run, PLUGIN-09 reported revision after it),
  environment cleared to the configured secret (PLUGIN-10), refusals that write nothing (ADR-008).
  Residual risk, stated rather than hidden: between hashing the executable and spawning it, a
  process with the operator's uid could replace the file; the existing executable rules (no
  group/other write, no symlink) limit that to the operator's own uid, and this slice adds no
  sandbox (§2 non-goals).
- **Security — ASVS applicability:** OWASP ASVS 5.0.0 as verification guidance, not a
  certification claim. V5.1 applies to the fail-closed validation of configuration, input and
  plugin responses (PLUGIN-01..03, PLUGIN-06, PLUGIN-08); V8.3.1 applies because the grant is
  rechecked by Kanban on every call rather than trusted from an earlier load (PLUGIN-07); V13.3
  applies to secret handling by reference (PLUGIN-15); V14.2.6 applies to minimal disclosure in
  listings and refusals (PLUGIN-12, PLUGIN-15).
- **Operability:** `plugin list` is the operator's single view of what is configured and why a
  plugin is unavailable (PLUGIN-12).
- **Performance:** no budget. Observation: each call hashes the executable and reloads the file,
  as the dispatcher already reloads the file per candidate (`rust/dispatcher.rs:680-681`); the
  owner accepted "more checks" for "no stale grant window" (`a-5f631e1d`).

## 7. Open questions

None. The two material questions the slice raised — which configuration file binds a plugin, and
when a binding is validated — were taken by the owner on `a-9056d954` and `a-5f631e1d`
(2026-10-01). The remaining shapes (version 2, the two pins, `plugin.read`, the verb names, the
canonical envelope) are wording choices under the decider, recorded with their rejected
alternatives in ADR-058.

## 8. Verification

Planned evidence for every mandatory requirement. The matrix section
(`## Requirements trace — docs/specs/plugin.md PLUGIN-01..PLUGIN-15`) is the trace of record; this
table is its draft and the two land identical. No test exists yet: the implementing row
`t-f73a81ae` writes each named test in the change that implements it, so every row says
`no e2e coverage` plainly.

| Requirement | Strength | Layer | Test name | Note |
| --- | --- | --- | --- | --- |
| `PLUGIN-01` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A2). |
| `PLUGIN-02` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A2). |
| `PLUGIN-03` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A2). |
| `PLUGIN-04` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A1). |
| `PLUGIN-05` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A6). |
| `PLUGIN-06` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A3). |
| `PLUGIN-07` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A4). |
| `PLUGIN-08` | MUST | unit | `none` | no unit coverage. Owed by `t-f73a81ae` (A3, A5). |
| `PLUGIN-09` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A5). |
| `PLUGIN-10` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A5). |
| `PLUGIN-11` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A3). |
| `PLUGIN-12` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A4). |
| `PLUGIN-13` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A7). |
| `PLUGIN-14` | MUST | unit | `none` | no unit coverage. Owed by `t-f73a81ae` (A1). |
| `PLUGIN-15` | MUST | process | `none` | no e2e coverage. Owed by `t-f73a81ae` (A4). |

## 9. Change log

- `2026-10-01` — slice created at `PLUGIN-01` .. `PLUGIN-15` under row `t-374806ea`, after owner
  verdicts `a-843bb89d` (slice admitted), `a-9056d954` (reuse `dispatchers.json`, synchronous
  entry point) and `a-5f631e1d` (shape at load, recheck per call). No supersessions yet.
- `2026-10-01` — independent `/quality spec` review (`ReviewPluginSpec1`, commit `79d3408`):
  SPEC-READY with three wording findings, applied without changing any requirement's meaning.
  PLUGIN-01 states the `timeoutMs` bound inclusively (1 to 300000, as the cited process rule
  accepts). PLUGIN-08 scopes the camelCase and unknown-field rules to the envelope fields and
  makes `input`/`output` opaque. PLUGIN-11 fixes number serialization to the parsed value, so
  "equal as JSON values" and "byte-identical" cannot disagree.
