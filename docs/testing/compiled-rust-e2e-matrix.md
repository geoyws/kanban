# Compiled Rust E2E matrix

The release gate runs `cargo test --test e2e` after Cargo builds the production
`kanban` executable. Each test invokes `CARGO_BIN_EXE_kanban` through
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
| Durable subscriptions | Separate compiled invocations migrate board schema v21; add, list, show, pause and resume a declarative subscription; observe its audited watch event without a secret reference; reject unknown selectors, raw-value-shaped secret references and duplicate identities; verify generated read-only/list metadata; and prove a second board cannot see the row. |
| Deployment ledger | Separate compiled processes prove capability ownership, idempotent start, exact served-commit success, derived current release, CLI/MCP parity, and real HTTP rendering of the cross-board matrix and attempt detail. |
| Deployment self-archive | A real archive process moves only old terminal non-current attempts out of default lists and hot partial indexes, keeps started and current successes hot, retains cold search and `--all` access, and proves a repeated sweep is idempotent. |
| Browser-backed operator approval | A real Chrome session loads the compiled `kanban serve` page, types into the live comment box, watches the quick labels switch to `Comment and Approve` / `Comment and Reject`, clicks a real submit button, and verifies the persisted decision/comment through the compiled CLI/store boundary. |

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
