# ADR-040: The GraphQL edge is a hand-rolled executor over apollo-compiler, not a framework

**Status:** Parked — GraphQL work stopped indefinitely by geoyws on 2026-09-07
(kanban `e-321f6350` note 118: "hand rolling our own graphql is a bad idea"),
hours after acceptance. The candidate rejections below stand on their
measurements. The Decision's plan does not: `apollo-compiler` 1.33.0 — the
pinned version — already ships a spec-following synchronous executor
(`resolvers::Execution::execute_sync`, `request::coerce_variable_values`,
introspection behind `enable_schema_introspection`), proven against a failing
non-null leaf under nullable and non-null parents by `probe-exec` in the spike
workspace (`t-38ee2070` note 117). If GraphQL is ever resumed, the executor is
the crate's; the hand-written surface is look-ahead prefetch, a depth walk, a
weighted complexity walk, and the `Int64` scalar with its coercion. Nothing else
in this document was revised after the stop.
**Date:** 2026-09-07
**Deciders:** geoyws, 2026-09-07, decision `a-dba26684` — "build it first, we need
the gql now" — selected and evidenced by claude@driver under epic `e-321f6350`,
task `t-38ee2070`. geoyws ruled that the work starts now; he did not pick the
framework himself and may supersede this choice by a later ADR.
**Supersedes:** nothing. This is the first ADR about the GraphQL edge.

## Context

Epic `e-321f6350` froze the V1 schema before the engine existed: 13 query roots
(`viewer`, `apiInfo`, `task`, `tasks`, `context`, `claimCandidates`, `search`,
`events`, `attention`, `handoffs`, `sitreps`, `tags`, `staleTasks`) and 21
mutation roots (`addTask`, `updateTask`, `moveTask`, `removeTask`,
`patchTaskMetadata`, `claimTask`, `heartbeatClaim`, `releaseClaim`, `addNote`,
`addCheckpoint`, `createHandoff`, `acceptHandoff`, `raiseAttention`,
`updateAttention`, `resolveAttention`, `reopenAttention`,
`reviseAttentionDecision`, `postSitrep`, `advanceStory`, `signoffStory`,
`unsignoffStory`), with exactly four request-scoped loader families
(`TaskGraphLoader`, `TaskThreadLoader`, `ClaimByTaskLoader`,
`TaskAttentionLoader`). Filter semantics follow Unum V2: recursive AND/OR/NOT,
sibling AND, whitelisted typed fields, ordered multi-sort, stable ID tie-break,
composite cursor, first+1 pagination, default 20, cap 100. The ceilings are
256 KiB request, 2 MiB response, depth 8, 64 aliases, 256 selected fields, 2,000
plan nodes, 64 filter nodes, logical depth 6, 16 OR branches.

The epic also fixed the seam: **the public `grust` API must accept a normalized
selection plan rather than expose Kanban types**, and the generic planner stays
independent of framework and application. The framework under selection is
therefore *only* the adapter between HTTP/GraphQL parsing and validation on one
side and `grust`'s normalized selection plan on the other. It is not the engine.

### The fact that decides most of it

Kanban's HTTP server is `tiny_http 0.12` with `default-features = false`
(`Cargo.toml:65`) and it is **synchronous**. There is no async runtime anywhere
in the dependency tree — no `tokio`, no `async-std`, no `smol`. The crate has 12
direct dependencies and zero GraphQL crates. Toolchain is Rust 1.96.1, edition
2024.

That is not a detail to be routed around. A framework whose execution entry
point returns a `Future` requires this crate to acquire and own an executor, and
— as measured in §2 below — a framework whose *DataLoader* depends on a
wall-clock batch window requires either a real async runtime or a per-request
latency penalty on every one of the four loader families the epic mandates.

### What the plan requires of the adapter

Introspection control (off for non-admin principals), typed error codes with no
internal leakage, depth *and* complexity hooks, DataLoader interoperability for
the four families, persisted operations as an allow-list of operation hashes for
the browser principal, and SDL generation so drift can be checked in CI.

## Candidates

Four candidates were built, not read about. Each implements the **same slice**
over the **same fixture**: query roots `task(id)`, `tasks(filter, orderBy,
first, after)` returning a connection, and `attention(status, first)` where each
row carries `choices` (a static one-to-many) and `task` (resolved through a
request-scoped loader that batches by task id).

**Citation convention.** A bare `<member>:NNN` below is
`/Users/geoyws/work/src/.gql-spike/<member>/src/main.rs:NNN`. `fixture:NNN` is
`/Users/geoyws/work/src/.gql-spike/fixture/src/lib.rs:NNN`. The scratch
workspace is outside any repository and is not part of this commit; it exists so
that every API claim in this document was compiled and run rather than inferred.

### The measurement setup

The fixture is 500 tasks and 50 attention rows shaped like the real rows —
`Task` mirrors `rust/model.rs:453` field for field (`fixture:19`), `Attention`
mirrors `rust/model.rs:822` (`fixture:44`). No SQLite: the spike measures the
adapter seam, not storage. 40 of the 50 attention rows are `open`
(`fixture:286`) so `attention(first: 20)` really yields 20 and the loader batch
really is 20 keys wide. Every attention row points at a distinct task
(`fixture:276`), so a correct loader must gather 20 distinct keys into one call.
`parent_id` points at a real earlier row (`fixture:245`) so the recursive
`parent` edge resolves and the depth ceiling can be tested with a *valid* deep
document instead of a field error.

The batch counter lives in the fixture, not in any candidate
(`fixture:222`–`fixture:229`), and increments once per **batch call** with a
separate max-batch-width gauge. What it records is therefore what the framework
asked for, not what a candidate's loader chose to do.

`baseline` is the control: the same fixture, the same response shape, hand-written
JSON, **no GraphQL layer at all**. It stops at a `serde_json::Value` exactly as
every candidate's `run` does, so `candidate − baseline` is the GraphQL layer and
nothing else. `hello` is a zero-dependency binary, the pure-Rust size floor.

Timings are p50 over 1,000 in-process iterations after a 100-iteration warmup
discard (`fixture:bench`, `fixture:449`), `--release`, no HTTP. Host: Apple M3
Max, `rustc 1.96.1 (31fca3adb 2026-06-26)`. Build time is a full `--release`
build of the member and its whole dependency graph into an empty
`CARGO_TARGET_DIR`, median of 4–5 samples.

A per-member *incremental* clean (`cargo clean --release -p <member>` with
dependencies left warm) was measured and then **discarded as a metric**,
because it is an artifact of ordering rather than of the crate: it reported
`cand-apollo` at 631 ms and `cand-minimal` at 1,839 ms purely because the first
member cleaned also forced a rebuild of the shared `fixture` path dependency,
which every later member then found warm. Only the full-graph figure above is
reproducible, so only it is quoted.

### 1. The evidence table

| | **async-graphql 7.2.1** | **juniper 0.17.1** | **minimal / graphql-parser 0.4** | **minimal / apollo-compiler 1.33** |
| --- | --- | --- | --- | --- |
| Runs on a sync server with no tokio | **Only with an executor.** `Schema::execute` returns a Future; there is no `execute_sync`. Driven here by `futures_executor::block_on` (`cand-async-graphql:476`). `tokio` is not a mandatory dep, but `async-trait` and `futures-util` are. Its DataLoader additionally needs a *spawner*, and with no runtime that spawner is a thread per batch flush (`cand-async-graphql:432`–`:433`). | **Yes, natively.** `juniper::execute_sync` returns the value directly, no executor (`cand-juniper:424`). | **Yes.** No async anywhere. | **Yes.** No async anywhere. |
| **Clean `--release` build of the member and everything under it**, empty target dir, median of 4–5 samples | **19.1 s** | 14.1 s | **5.7 s** | 8.8 s |
| Transitive crates | **94** | 93 | **29** | 75 |
| `--release` binary | 3,567,184 B | 1,717,648 B | 986,032 B | 1,940,624 B |
| — delta vs `hello` (465,928 B) | **+3,101,256 B** | +1,251,720 B | **+520,104 B** | +1,474,696 B |
| — delta vs `baseline` (626,648 B) = GraphQL layer | **+2,940,536 B** | +1,091,000 B | **+359,384 B** | +1,313,976 B |
| **p50 `tasks(first:20)`**, 20 fields + filter + 2-key sort | 162.4 µs | 160.1 µs | **73.5 µs** | 107.3 µs |
| **p50 `attention(first:20){choices task{id title}}`** | **1,427.3 µs** | 108.9 µs | **46.5 µs** | 59.2 µs |
| p50 single `task(id)` read | 10.6 µs | 4.9 µs | 3.5 µs | 8.7 µs |
| (control) `baseline`, same work, no GraphQL | — | — | tasks 60.8 µs / attention 53.8 µs | — |
| **Loader: batch calls per request / keys / max width** | **1 / 20 / 20** — but only with a 1 ms batch window | 1 / 20 / 20 | 1 / 20 / 20 | 1 / 20 / 20 |
| Loader with the batch window removed | **Batching becomes a race.** `delay(Duration::ZERO)` (`cand-async-graphql:438`) gave 3–10 batch calls with max widths 3–15 for the same one-request 20-key query across 10 release runs — never reliably 1 | n/a | n/a | n/a |
| Loader mechanism | First-party `async_graphql::dataloader::DataLoader` + `Loader` impl (`cand-async-graphql:79`–`:83`), `load_one().await` per sibling field (`cand-async-graphql:274`) | **None shipped.** Hand-rolled look-ahead prefetch at the *parent* resolver (`cand-juniper:394`–`:398`) | Natural: the executor owns the walk, so keys are gathered from the plan before any row is projected (`cand-minimal:550`–`:551`) | Same as left (`cand-apollo:360`) |
| **Selection exposure (the seam)** | `ctx.field() -> SelectionField`, then `.name()`, `.alias()`, `.arguments()`, `.selection_set()` — total, 12 lines (`cand-async-graphql:32`–`:42`) | `executor.look_ahead() -> LookAheadSelection`, then `.field_name()`, `.field_alias()`, `.arguments()`, `.children()` (`cand-juniper:41`–`:49`). Values arrive **already coerced**, so the literal type is gone and the plan must re-probe via `try_to_bool/int/float` (`cand-juniper:59`–`:68`); every child is wrapped in `Spanning`, so the walk unwraps `.item` at each level (`cand-juniper:73`–`:75`) | The plan **is** the parse output; no translation exists to drift (`cand-minimal:233`) | The plan is the walk output over a *validated* document, so the walk has no error path at all (`cand-apollo:167`) |
| Introspection control | `SchemaBuilder::disable_introspection()` (`cand-async-graphql:444`). **Measured: it does not refuse the document, it resolves `__schema` to null** — `{"data":{"__schema":null}}` | `RootNode::disable_introspection()` (`cand-juniper:474`) — **refuses** with `GraphQL introspection is not allowed, but the operation contained __schema` | Off by construction: meta-fields are absent from the schema table, so `__schema` fails "Fields on Correct Type". Turning it *on* is the work | `__schema` is a valid field, so refusal is by policy (`cand-apollo:472`) → `INTROSPECTION_DISABLED` |
| Typed error customization | `Error::new(..).extend_with(..)`, setting `code` in the closure (`cand-async-graphql:67`–`:68`). **But framework-generated errors carry no `extensions.code`** — depth and validation failures surface as bare messages (`Query is nested too deep.`, `Unknown field "nope" on type "Query".`), so a typed code on *every* error needs an extension pass | `FieldError::new(msg, graphql_value!({"code": ..}))` (`cand-juniper:86`). Document-level errors bypass it | Total control; one constructor, one call-site file (`cand-minimal:46`) | Total control; apollo diagnostics truncated to one line so source snippets never reach a client (`cand-apollo:455`–`:457`) |
| Depth limit | **First-party** `limit_depth(8)` (`cand-async-graphql:442`) → `Query is nested too deep.` | **None shipped.** Hand-rolled walk (`cand-juniper:82`) and it must be repeated in **every** root resolver — 13 queries + 21 mutations (`cand-juniper:326`–`:327`, repeated at `:344`) | Trivial over the plan (`cand-minimal:351`) → `DEPTH_LIMIT` | Trivial over the plan (`cand-apollo:196`) → `DEPTH_LIMIT` |
| Complexity limit | **First-party** `limit_complexity(2_000)` (`cand-async-graphql:443`) | **None shipped** | Field/alias/plan-node counts over the plan | Field/alias counts over the plan (`cand-apollo:196`–`:214`) |
| Persisted-operation hook | `Extension::prepare_request` (`cand-async-graphql:388` declares `PersistedOnly`, `:404` is the hook). **Measured both ways:** empty allow-list refuses (`document is not in the persisted-operation allow-list`), allow-list containing the hash admits and returns 20 edges | No hook needed — hash the body before `execute_sync` | Hash gate before the parser runs, so an unknown document costs one sha256 (`cand-minimal:638`) → `OPERATION_NOT_PERSISTED` | Same (`cand-apollo:440`) → `OPERATION_NOT_PERSISTED` |
| SDL export | `Schema::sdl()` (`cand-async-graphql:556`), 1,813 B | `RootNode::as_sdl()`, 1,824 B — **behind the non-default `schema-language` feature**; juniper 0.17.1 declares no `default` key at all | Written from the same table that validates, so validator and SDL cannot disagree (`cand-minimal:180`), 1,007 B | **The SDL is the input.** `Schema::parse_and_validate(SCHEMA_SDL, ..)` (`cand-apollo:107`); round-trip re-serialize proved **byte-stable** |
| Spec validation rules (of 30, October 2021) | 30 — framework | 30 — framework | **3.** Fields on Correct Type, Leaf Field Selections, Argument Names. **27 to hand-roll**; the count is asserted at startup, not claimed in a comment | **30 — vendor pass**, `ExecutableDocument::parse_and_validate` (`cand-apollo:449`), 0 hand-rolled |
| Last release / MSRV / open issues | 7.2.1 stable; `8.0.0-rc.5` 2026-04-21 is a **release candidate**; last default-branch commit 2026-04-21; MSRV 1.86 on the 8.0 cycle; 225 open issues | 0.17.1, 2026-01-26; last commit 2026-08-30; MSRV 1.85; 86 open issues | 0.4.x, **quiet since 2025-01**; no validation and none planned | 1.33.0, **2026-09-03** (4 days before this ADR); 761,065 all-time / 154,943 recent downloads |
| **What it forces on `grust`** | Nothing, *if* wrappers are used — proved: every object here wraps `&'static fixture::Task` and the fixture types carry no derive and no framework trait. The cost is a translation from `SelectionField` that must be maintained | Same wrapper escape hatch. **But** taking `executor` to reach `look_ahead()` forces `scalar = juniper::DefaultScalarValue` on the root (`cand-juniper:315`), pinning the schema's scalar | Nothing. The plan type is ours | Nothing. The plan type is ours |

### 2. The two findings that are not in any changelog

**async-graphql's DataLoader trades correctness for latency on a synchronous
server.** With the default 1 ms batch window it batches perfectly — 1 call, 20
keys, max width 20 — and the `attention` query costs **1,427.3 µs p50**, which
is 27× `baseline` (53.8 µs) and 31× the minimal adapter (46.5 µs). Set
`delay(Duration::ZERO)` to remove the penalty and batching **stops being
deterministic**: across 10 release runs of the identical single request, the
same 20 keys arrived as 9, 7, 7, 9, 10, 3, 7, 6, 6 and 7 separate batch calls,
with maximum batch widths of 3 to 15. Not once was it reliably one call. The
window is not a tuning knob; it is how the DataLoader collects a batch, and it
works because a cooperative async runtime polls every sibling field before the
timer fires. With
`futures_executor::block_on` and a thread-per-flush spawner there is no such
guarantee, and the race is visible in the counter. A non-deterministic number of
database round-trips per request is worse than a fixed 1 ms penalty, because it
cannot be capacity-planned and it will not reproduce in a test. The epic mandates four
request-scoped loader families, so this is the central path, not an edge case.
Getting reliable batching out of async-graphql means adopting a real async
runtime — which is the change to kanban's runtime model that the sync-server
fact was warning about.

**juniper cannot represent kanban's timestamps.** `DefaultScalarValue` has no
64-bit integer. Every kanban timestamp is `i64` unix milliseconds
(`rust/model.rs:470`–`rust/model.rs:472`), and `created_at`/`updated_at` are
also ordering and cursor keys. In the spike they are exposed as `Float`
(`cand-juniper:141`) — safe numerically below 2^53, wrong in the schema, and it
makes `createdAt: Float!` a permanent wart on an ordering key. Fixing it
properly means a custom `ScalarValue`, which then collides with the
`scalar = DefaultScalarValue` pin that taking `executor` already forced. That is
on top of hand-rolling the DataLoader and both ceilings.

### 3. What each candidate would still owe

A hand-rolled executor is not free, and the honest list matters more than the
table. Beyond the 30 validation rules — which apollo-compiler supplies —
execution semantics remain ours: null propagation and error bubbling to the
nearest nullable parent (spec §6.4.4), `@skip`/`@include` execution, variable
coercion into the plan (the spike leaves `Value::Variable` as `Null`,
`cand-apollo:151`), per-field error collection with `path`, and
`__typename`. Introspection itself becomes ours to implement if an admin
principal is ever to receive it. Estimate for V1, which has no interfaces, no
unions and no subscription root: roughly 600–900 lines of executor plus tests,
and a further ~300 lines if served introspection is wanted later. That work is
deferrable because V1 disables introspection for non-admin principals; it is not
avoidable if it is ever wanted.

The `graphql-parser` variant additionally owes the 27 validation rules it does
not implement, enumerated in `cand-minimal:294`–`:322`: Executable Definitions,
Operation Type Existence, Operation Name Uniqueness, Lone Anonymous Operation,
Single Root Field, Field Selection Merging, Argument Uniqueness, Required
Arguments, Fragment Name Uniqueness, Fragment Spread Type Existence, Fragments
on Object/Interface/Union Types, Fragments Must Be Used, Fragment Spread Target
Defined, Fragment Spreads Must Not Form Cycles, Fragment Spread Is Possible,
Values of Correct Type, Input Object Field Names, Input Object Field Uniqueness,
Input Object Required Fields, Directives Are Defined, Directives Are in Valid
Locations, Directives Are Unique per Location, Variable Uniqueness, Variables
Are Input Types, All Variable Uses Defined, All Variables Used, All Variable
Usages Are Allowed. Several are security surfaces rather than ergonomics:
fragment-spread cycles are an unbounded-expansion denial of service, and the
variable-usage rules are the type-safety boundary for every mutation argument.
Hand-rolling 27 spec rules correctly is the single largest avoidable risk in
this whole design, and it is avoidable.

The apollo variant refuses each of those with a typed code, measured, in one
pass and with **zero** hand-rolled rules: fragment cycle → `VALIDATION_FAILED`,
undefined fragment → `VALIDATION_FAILED`, wrong value type
(`tasks(first: "twenty")`) → `VALIDATION_FAILED`, undefined variable →
`VALIDATION_FAILED`, duplicate operation name → `VALIDATION_FAILED`, unknown
directive → `VALIDATION_FAILED`.

## Decision

**The GraphQL edge is a hand-rolled synchronous executor over
`apollo-compiler` 1.33**, producing `grust`'s normalized selection plan
directly. No GraphQL framework is adopted.

The reasons, in order of weight:

1. **The sync-server fact rules out async-graphql.** Kanban is synchronous
   `tiny_http` with no async runtime. async-graphql has no synchronous execution
   entry point at all, and its DataLoader — required four times over by the epic
   — batches reliably only behind a wall-clock window that costs 1,427.3 µs p50
   on the loader query, and degrades to a non-deterministic 3–10 calls per
   request when the window is removed.
   Adopting it means adopting an async runtime for a synchronous server, plus 94
   crates and 2.94 MB, to obtain a loader that a 20-line function already
   provides correctly.
2. **The only real reason to take a framework was the 30 validation rules, and
   apollo-compiler supplies them without the framework.**
   `ExecutableDocument::parse_and_validate` is a vendor validation pass over a
   schema parsed from SDL, synchronous, no runtime, no object traits. It
   measurably refuses every rule class the hand-rolled validator misses. The
   framework's decisive advantage is available à la carte.
3. **The seam the epic mandates becomes native instead of translated.** `grust`
   must accept a normalized selection plan and stay framework-independent. Both
   frameworks can feed such a plan — proved, `cand-async-graphql:32` and
   `cand-juniper:41` — but each does so through a translation from the
   framework's own selection representation, which is a permanent surface that
   can drift and that carries the framework's quirks (juniper's pre-coerced
   values and `Spanning` wrappers). With apollo the plan is what the walk
   produces from an already-validated document, and the walk has no error path.
4. **Drift stops being detected and becomes unrepresentable.** apollo validates
   against SDL *text*. The checked-in `docs/graphql/schema.graphql` is therefore
   not a rendering of the server's schema that could disagree with it — it *is*
   the schema the server enforces. The round-trip was measured byte-stable. Both
   frameworks invert this: the Rust types are the source and the SDL is
   generated, so the checked-in file is a copy that must be policed.
5. **The ceilings are policy, not spec, and belong over the plan.** Depth 8, 64
   aliases, 256 fields, 2,000 plan nodes, 64 filter nodes, logical depth 6, 16 OR
   branches — one function over one data structure, applied once
   (`cand-apollo:196`). juniper ships neither a depth nor a complexity limiter
   and would need the check repeated across 34 roots.
6. **Weight and speed, in the right direction.** 75 crates and +1.31 MB against
   94/+2.94 MB and 93/+1.09 MB; full-graph build 8.8 s against 19.1 s and
   14.1 s; 107.3 µs / 59.2 µs against 162.4 µs / 1,427.3 µs and
   160.1 µs / 108.9 µs. The `attention` figure lands within 5.4 µs of the
   no-GraphQL control.
7. **juniper is the runner-up and loses on three hand-rolls plus a schema
   defect**, not on one thing: no DataLoader, no depth limit, no complexity
   limit, and no 64-bit integer for timestamps that are also cursor keys.

**Why not the `graphql-parser` variant, which is faster and lighter still.** It
is faster (73.5 µs / 46.5 µs) and much lighter (29 crates, +359 KB), and it
wins on independence. It loses on validation completeness, and that is the one
axis where losing is not a trade — it is a correctness and denial-of-service
hole 27 rules wide on a public-facing API. The ~1 MB and ~34 µs that
apollo-compiler costs buys the entire October 2021 validation section from a
vendor who publishes it as a product. That is the right purchase.

**What was given up.** Field-level execution concurrency (irrelevant on a
synchronous server, where a request already owns its thread). A vendor-supplied
executor, with its null-propagation and directive semantics — we now owe those,
scoped in §3. Served introspection, which must be implemented if an admin
principal is ever to receive it; V1 has it off for non-admin principals, so the
cost is deferred rather than paid. A subscription runtime, which V1's schema does
not have a root for. And the ability to say "the framework handles it" about any
future execution-semantics question: from here, execution is ours.

## Consequences

`grust` stays a public crate (MIT OR Apache-2.0) whose API accepts
`SelectionPlan` and returns a SQLite plan, with no GraphQL type in its
signatures — which is now easy to hold, because no GraphQL framework type exists
anywhere in the tree to leak. The plan type demonstrated here
(`fixture:107`–`fixture:122`) is framework-free by construction: field, alias,
ordered arguments, children, and nothing else.

Kanban's dependency tree gains `apollo-compiler` and stays synchronous. No
`tokio`, no `async-trait`, no executor, and `tiny_http`'s handler keeps calling a
function that returns a value. This is the property that makes the edge
reviewable: a request is a function call, and a stack trace is a stack trace.

The parse-and-validate cost is now explicit and measurable at
107.3 µs / 59.2 µs p50 for the two benchmark documents, against 60.8 µs / 53.8 µs
for hand-written JSON over the same fixture. Persisted operations make this
cheaper in the common path, not more expensive: the hash gate runs *before* the
parser (`cand-apollo:440`), so a browser request that is not on the allow-list
costs one sha256 and never reaches validation.

The 27 validation rules we are not writing are also 27 rules we are not
testing, not reviewing and not getting wrong. In exchange we own the executor,
and the items in §3 are real follow-on work that must be tracked on the epic
rather than discovered later. Specifically: null propagation, `@skip`/`@include`,
variable coercion, and per-field error paths are required before the edge can be
called spec-conformant, and none of them is exercised by the spike.

`apollo-compiler` 1.33.0 shipped 2026-09-03 and a `2.0.0-beta` line is open. We
pin to 1.33 and treat the 2.0 migration as a scheduled cost, not a surprise. The
crate has a known validation gap (apollo-rs #1027, directive arguments on unused
fragments), which is a reminder that a vendor's pass is not spec-perfect — it is
merely enormously better than 3 of 30.

Introspection being off is now a *policy* check rather than a schema property
(`cand-apollo:472`), because `__schema` is a legitimate field of a valid schema.
That check must therefore be tested, since nothing structural prevents a
regression from serving it.

## Schema drift check

The checked-in SDL is the schema the binary enforces, so the drift test's job is
to prove that the file on disk, the schema the process serves, the root list the
epic froze, and the persisted-operation manifest are all one thing.

### Artifacts

- `docs/graphql/schema.graphql` — the SDL. **Not generated from Rust types**: it
  is `include_str!`-embedded and handed to
  `Schema::parse_and_validate` at startup, so a malformed or drifted file fails
  the process, not just CI.
- `docs/graphql/operations/*.graphql` — one persisted operation document per
  file, the allow-list for the browser principal.
- `docs/graphql/operations/manifest.json` — `{formatVersion, operations: [{name,
  file, sha256, bytes}]}`, sorted by `name`, where `sha256` is over the operation
  file's exact bytes. This is the allow-list the browser principal is checked
  against.

### The command surface it is checked through

Two read-only subcommands, so the test can interrogate the **built binary**
rather than link the library and check itself:

- `kanban graphql schema` — prints the SDL the running process validates
  against. It MUST print `schema.to_string()`, the **re-serialization of the
  parsed schema**, and never echo the `include_str!` bytes. Echoing the
  embedded string would make assertion 1 below a comparison of a file with
  itself: a test that can never fail and therefore proves nothing. Printing the
  re-serialization is what makes the assertion bite, and it is sound because
  the round-trip was measured byte-stable in the spike (`cand-apollo:554`–`:557`).
- `kanban graphql operations` — prints the manifest the running process gates on.

### The test

One test, in `tests/e2e.rs`, following the house naming of
`hig_release_script_enumerates_exactly_the_executables_the_crate_declares`:

```
graphql_schema_sdl_and_operation_manifest_match_the_served_schema
```

It asserts, in this order, failing on the first:

1. **Byte identity of the schema.** `kanban graphql schema` stdout equals
   `docs/graphql/schema.graphql` byte for byte. Because the binary re-serializes
   the schema it parsed, this catches a hand-edit that is semantically
   equivalent but differently formatted — the round-trip was measured stable, so
   canonical bytes are a fair thing to demand.
2. **Root counts.** The served schema's `Query` type has **exactly 13** fields
   and `Mutation` **exactly 21**. An accidental extra root fails here, which is
   the specific failure this clause exists for.
3. **Root names.** The two field-name sets equal the frozen lists quoted in the
   Context section above, compared as sorted sets so a rename fails as loudly as
   an addition.
4. **Executor coverage.** Every `Query`/`Mutation` field name in the SDL has a
   dispatch arm, and every dispatch arm has an SDL field. This is the drift that
   byte-comparison cannot see: an SDL field with no arm resolves to `null` and
   looks like missing data, and an arm with no SDL field is dead code.
5. **Every persisted operation is valid against the served schema.** Each
   `docs/graphql/operations/*.graphql` parses and validates against the SDL from
   step 1 — so an operation that outlived a schema change fails CI instead of
   failing a browser.
6. **Manifest identity.** `kanban graphql operations` stdout equals
   `docs/graphql/operations/manifest.json` byte for byte, and each entry's
   `sha256` and `bytes` match a fresh read of its file. A new operation file with
   no manifest entry, or an edited operation whose hash was not regenerated,
   fails here.

Exact command:

```sh
cargo test --locked --test e2e -- --exact \
  graphql_schema_sdl_and_operation_manifest_match_the_served_schema --nocapture
```

Intentional schema or operation changes are landed by regenerating in the same
commit, which writes both artifacts and then re-runs the identical assertions:

```sh
KANBAN_GRAPHQL_BLESS=1 cargo test --locked --test e2e -- --exact \
  graphql_schema_sdl_and_operation_manifest_match_the_served_schema
```

`KANBAN_GRAPHQL_BLESS` writes `docs/graphql/schema.graphql` and
`docs/graphql/operations/manifest.json` from the binary and then asserts as
normal, so a blessed run that still fails is a real failure rather than a
laundered one. Blessing is never done from CI.

## References

- `Cargo.toml:65` — `tiny_http 0.12` with `default-features = false`; the
  synchronous server this whole decision turns on
- `rust/model.rs:453` — `Task`, the 20 fields the benchmark query selects
- `rust/model.rs:470`–`rust/model.rs:472` — the `i64` unix-millisecond timestamps
  juniper's `DefaultScalarValue` cannot represent
- `rust/model.rs:822` — `Attention`, the loader-bearing row
- [ADR-039: The release manifest and receipt schema is frozen at formatVersion 1](ADR-039-release-manifest-and-receipt-schema.md)
  — the checked-in-artifact-plus-drift-test shape this ADR's drift check follows
- [ADR-010: Adapters generated from the command surface](ADR-010-adapters-generated-from-the-command-surface.md)
  — a surface is described once, as data; here the SDL is that description
- [ADR-008: Fail closed on ambiguous and destructive operations](ADR-008-fail-closed-on-ambiguous-and-destructive-operations.md)
  — the refusal form the typed error codes follow
- GraphQL specification, October 2021, Validation —
  <https://spec.graphql.org/October2021/#sec-Validation>; 30 rules, of which the
  `graphql-parser` variant implements 3
- `apollo_compiler::ExecutableDocument::parse_and_validate` —
  <https://docs.rs/apollo-compiler/1.33.0/apollo_compiler/executable/struct.ExecutableDocument.html>
- `async_graphql::dataloader::DataLoader` —
  <https://docs.rs/async-graphql/7.2.1/async_graphql/dataloader/struct.DataLoader.html>
- `juniper::execute_sync` —
  <https://docs.rs/juniper/latest/juniper/fn.execute_sync.html>
- Spike workspace (outside any repository, not part of this commit):
  `/Users/geoyws/work/src/.gql-spike/` — 6 members, `run.sh`, `results.json`;
  every API claim above cites a file and line in it
- Kanban board: epic `e-321f6350`; task `t-38ee2070` (this selection); decision
  `a-dba26684` (2026-09-07, build it now)
