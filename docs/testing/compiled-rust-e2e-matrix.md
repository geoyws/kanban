# Compiled Rust E2E matrix

The release gate runs `cargo test --test e2e` and
`cargo test --test dispatcher_e2e` after Cargo builds the production
executables. Each test invokes its `CARGO_BIN_EXE_*` binary through
`std::process::Command`; no test calls Kanban domain functions in-process.

| Requirement | Process-boundary evidence |
| --- | --- |
| SQLite persistence and restart | Separate `init`, `task add`, `note`, `claim`, `handoff`, `context`, and `checkpoint` processes reopen the same board. |
| Multiple worktrees | A second directory attaches to a named board and reads/writes the same board; `workspace list` includes rootless boards and dashboard reports the roots as hints. |
| Atomic ownership | Two compiled processes race to claim one task; exactly one exit status may succeed. |
| Token-pressure handoff | The outgoing process creates the structured handoff, releases its lease, and an incoming process accepts with a different token. |
| Stale-token exclusion | The outgoing token is used for a post-handoff heartbeat and must fail. |
| Secret-safe read models | `task show` and rendered context are checked for both the literal token and the `leaseToken` field name. |
| atmux JSON import | A real source file containing epic, story, and task hierarchy is imported through the compiled CLI and the parent link is read back. |
| atmux SQLite import | A real legacy SQLite database is created and imported by a separate CLI process; duplicate insert-only import is rejected, then explicit reconciliation refreshes the existing rows and reports created/updated counts. |
| Operations | Dashboard counts, `doctor` integrity and rootless advisory reporting, multi-board backup, rootless restore, and reopening a copied board through `--db` are exercised. |
| Board-selector applicability | Compiled processes are refused by name for every selector every operation declares it discards, driven from `kanban schema --json` rather than restated; `doctor --db`, `backup --db` and the registry event trail are asserted directly, the selectors `init`, `workspace attach` and the board commands honour still resolve, and a manifest-wide sweep proves no operation exits zero while discarding a selector. Every selector the manifest does *not* list as ignored is passed a valid value on each read-only positional-free operation and must be honoured, which catches a command that refuses a selector it never declared. A separate process holds the data-root flock and proves `restore` still contends for it exclusively, and `doctor`/`backup` for it shared, when `KANBAN_DB` names a board outside the data root that the command discards. |
| Audit safety | Separate compiled processes create board and registry history, verify clean chains, and then reject edited, deleted and reordered event rows. Manifested backup/restore tests reject a substituted database, preserve a manifested rescue snapshot, keep archive continuity, and detect an intact older database against a retained newer anchor. |
| Existing-format compatibility | The binary opens a separately created `user_version=3` database matching the released TypeScript task schema without migration/export. |
| Pull routing and task graph | Separate CLI processes exercise `claim --next`, priority/dependency readiness, lane preference, role filtering, driver scope, assignee gates, and cycle rejection. |
| Read-only scheduler inspection | A compiled `claim --candidates --project` process excludes dependency-blocked, draft-ancestor, container, leased, incompatible-assignee and driver-only rows; byte, timestamp and row-count receipts prove no board or registry write, and every returned row is then accepted by the atomic claim path. |
| Story lifecycle | Separate processes exercise planning through done, child-lane gates, epic activation, review signoff/revocation, reviewer/committer dispatch, and merge completion. |
| Bounded projections | Long append-only history is rendered within the requested context bound while preserving the newest next action; generated TODO output declares SQLite authority. |
| SQLite-native RAG retrieval | Separate processes prove exact-ID top-one, the five-query paraphrase corpus, filters, cold-history opt-in, cross-board isolation, bounded cited results, explicit vector rebuild, and V12-to-V13 knowledge preservation. |
| MCP search parity | The generated `search` read tool and `search_rebuild` write tool execute the real CLI over stdio and return a cited source. |
| Served search | A real loopback HTTP conversation retrieves a cited cross-board result; byte comparison and source guards prove the pages expose no board mutation. |
| Cursor-native watch | Compiled-process tests cover literal-0 bootstrap, additive protocol-v1 payload and redaction, repeatable semantic predicates before the delivery limit, full normalized cursor binding, fail-closed selectors, malformed or future cursors, heartbeat delivery across read-only reopen cycles, and replay of removed subjects and historical relation targets. |
| Durable subscriptions | Separate compiled invocations migrate a pre-subscription board through declaration schema v21 to current schema v23, with subscriptions entering at v21 and dispatcher state at v22; add, list, show, pause and resume a declarative subscription; observe its audited watch event without a secret reference; reject unknown selectors, raw-value-shaped secret references and duplicate identities; verify generated read-only/list metadata; and prove a second board cannot see the row. |
| Bounded activity window | A real compiled-process `events/ev` read uses `--after START_MS --before END_MS` as a half-open `[after,before)` millisecond window, keeps SQL filters ahead of `--limit`, includes archived rows only with `--all`, and fails closed on time flags in registry/rule scope. |
| Durable subscription dispatcher | The dedicated compiled `kanban-dispatcher` process resolves each explicit board selector, targets one consumer, proves missing host-local allow-listed configuration fails at startup before the first materialization or claim, invokes a separately compiled deterministic fake adapter, clears inherited environment, injects only the named secret, and persists exact success/failure attempt state. Competing worker processes produce one claim/invocation; pause/resume is rechecked; exit, malformed, mismatched, oversized and timeout responses keep stable error codes; SIGTERM stops idle polling and a running adapter; and a real post-success/pre-ack process crash recovers after lease expiry with two invocations and durable `lease_expired` then `success` attempts, proving at-least-once rather than exactly-once delivery. This is the generic dispatcher gate, separate from the Codex queue bridge. |
| Codex queue adapter-contract | Separate compiled-process adapter-contract coverage with a fake Codex executable proves the bridge boundary only: argv selection, fixed `CODEX_HOME`, environment clearing, unrelated child acknowledgement, and an adapter-derived response. Focused unit tests prove exact version/help drift rejection, trusted path identity, and bounded input/output handling. Neither layer proves installed Codex support. |
| Codex app-server adapter-contract | Separate compiled-process adapter-contract coverage against a dependency-free fake Codex executable proves the experimental, opt-in read-only bridge boundary only: consumer `codex.app-server`, action `start-readonly-turn`, capability `start`, a child environment cleared to exactly `CODEX_HOME` and a fixed `PATH=/usr/bin:/bin` on both the probe and app-server spawns, version/help probes, private tempdir schema generation and verification, one accepted completion, and `AdapterResponse`-only stdout. Focused unit tests prove exact version/help/schema-hash drift rejection, trusted path identity, and bounded input/output handling. The distinct HAX live smoke against installed Codex/model remains the live check for this bridge; no pass claim is made here. |
| Codex queue live smoke | The separately named HAX live smoke receipt against a separately owned idle test session in a disposable workspace is the distinct live check for installed Codex support, the exact ingress path, and one received queued message. It is a smoke receipt, not compiled-process E2E. |
| Deployment ledger | Separate compiled processes prove capability ownership, idempotent start, exact served-commit success, derived current release, CLI/MCP parity, and real HTTP rendering of the cross-board matrix and attempt detail. |
| Deployment self-archive | A real archive process moves only old terminal non-current attempts out of default lists and hot partial indexes, keeps started and current successes hot, retains cold search and `--all` access, and proves a repeated sweep is idempotent. |
| Browser-backed operator approval | A real Chrome session loads the compiled `kanban serve` page, types into the live comment box, watches the quick labels switch to `Comment and Approve` / `Comment and Reject`, clicks a real submit button, and verifies the persisted decision/comment through the compiled CLI/store boundary. |

Browser discovery for that gate is ordered as `KANBAN_CHROME`, then the existing platform, `PATH`, and fixed-system candidates, then the newest executable Playwright Chromium under `XDG_CACHE_HOME/ms-playwright` or `HOME/.cache/ms-playwright`. Chromium sandboxing stays enabled for non-root launches and is disabled only when the effective UID is `0`, because upstream Chrome refuses sandboxed root.

Passing library/unit tests or invoking `rust/main.rs` through an interpreter is
not E2E evidence. The gate is incomplete until the compiled executable passes
this matrix on a clean test data directory, including the real-browser path
above.

## Watch coverage note

- The watch slice is coverage-driven, not count-driven.
- It covers literal-0 bootstrap, persisted opaque cursors, malformed and future
  cursor rejection, selector and scope mismatch rejection, heartbeat delivery
  across read-only reopen cycles, additive protocol-v1 payload shape and
  redaction, repeatable semantic filters, sparse matches before `--limit`,
  removed-subject replay, and historical relation-target replay.
- This matrix does not claim deployment evidence or full-suite release status.
