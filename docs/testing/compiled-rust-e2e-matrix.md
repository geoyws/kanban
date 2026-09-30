# Compiled Rust E2E matrix

The gate is one command:

```bash
scripts/release-gate.sh
```

That script IS the gate. Nothing else is, and no list here restates its
steps: it runs
`cargo fmt --all -- --check`,
`cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked --lib`, the two ignored
fixed-descriptor remap unit tests serially, the pinned `skills/kb` package's
own `bash skills/kb/tests/kb-wrapper-tests.sh`, and then every integration
target one at a time. Read the script for the order and the reasons; what
follows is only what a reader of this matrix needs to know about the Rust
half of it.

**The integration targets are the eleven in `tests/`**, in the order the
script runs them — cheapest first, `e2e` last:
`claude_print_adapter_e2e`, `codex_queue_adapter_e2e`,
`access_refusals_e2e`, `opencode_adapter_e2e`, `kimi_acp_adapter_e2e`,
`cursor_worker_adapter_e2e`, `zcode_notify_adapter_e2e`, `dispatcher_e2e`,
`codex_app_server_adapter_e2e`, `authz_bypass_matrix_e2e`, and `e2e`. Each
invokes the relevant production `CARGO_BIN_EXE_*` binaries through
`std::process::Command`; those process-boundary assertions are
compiled-process evidence. The gate as a whole is a
unit/integration/process gate, not compiled-process E2E.

**`authz_bypass_matrix_e2e` runs as non-root only.** The managed broker
refuses root pairs by design (`rust/routing.rs` `local_authority` mints no
authority for euid 0; `rust/policy.rs` refuses root bootstrap/prove-rebind
pairs), so as uid 0 every managed command in that target answers
`denied-or-not-found`. The suite fails fast with that sentence instead of
failing test by test, and it never skips: run it as a normal user or in the
Linux gate container.

**Serialization is a rule, not a preference.** Each target runs as its own
`cargo test --locked --test TARGET -- --test-threads=1`, and no cargo
command runs concurrently with another. Some cases drive a real Chrome
against loopback listeners they own; two of those at once contend for
one browser cache, ephemeral ports and the whole machine, and what that
produces is a flake that reads like a product bug. A single
`cargo test --all-targets` is therefore NOT this gate.

**`KANBAN_CHROME`** is passed through by the script and is the first entry
in the browser discovery order below. It is how a host whose system Chrome
is broken still runs the real-browser evidence. A value naming a
non-executable path is refused by the script rather than silently resolving
to some other browser.

**Run it on Linux, in a container, holding a gate slot.** Kanban deploys to
Linux, so a gate receipt comes from Linux: on the MacBook Pro that is

```bash
scripts/container-gate.sh WORKTREE
```

which runs this same script unchanged inside the image
`scripts/container-gate.Dockerfile` builds, with the clean candidate
checkout mounted read-only, and prints the candidate SHA and image ID for
the receipt. It holds one of the host's medic `gate-slot` slots for as long
as the container runs, so parallel lanes queue instead of overloading the
machine. `--loop TARGET TEST --iterations N` runs one test N times to
measure a flake; that count is investigation, never gate evidence. The first
Linux runs (2026-09-24, `t-a3928b36`) found a test no macOS gate had ever
compiled and a delivery race macOS never showed, which is the reason.

**The standing constraint on all of it:** the gate goes green because the
system became true, never because a measurement was loosened. Too slow
means make it faster or serialize it — never sample it. There is no
`--only`, no `--skip`, no quick mode and no environment variable that turns
a step off, and adding one would be the loosening this sentence exists to
forbid.

What the gate does not run, said plainly so a reader does not mistake it
for sampling: nothing. No case in `tests/` is `#[ignore]`d. The last one —
the `t-4b9501b3` write-refusal shape — was resolved on 2026-09-23 by
`t-97d8d0b9` correcting the contract, and now runs as
`the_write_refusals_answer_the_contracts_html_error_paragraph_over_http`.
The three-surface aggregate-listing finding now runs as compiled-process evidence.

Measured once end to end on 2026-09-20 on `@@mbp` (darwin-arm64, M3 Max)
with `KANBAN_CHROME` pointed at Playwright Chromium: 17 steps green in
44m10s, of which the `e2e` target is 2338s of 415 serialized cases. That
is the number to make smaller by making it faster, not by cutting it up.

Addendum 2026-09-29 (t-2b6a496e): the estate norm moved on 2026-09-28
(migration epic e-0ea4e50b; placement in dotfiles/infra-root, never here):
Unum and geoyws gates run on the estate test host inside the Linux gate
image. First green there: commit `ee8d062`, 17 steps, `e2e` 319/0
(image `kanban-gate:1.95-chrome-u0`, inner user `nobody`). The 2026-09-20
`@@mbp` measurement above stays as history.

The two fixed-descriptor remap unit tests are isolated unit evidence, not
compiled-process E2E; they are `#[ignore]`d and are run serially and in
isolation by the gate:

```bash
cargo test --lib --locked workspace_adopt_fd_remap_handles_ -- --ignored --test-threads=1
```


Coverage over the whole Rust tree is collected separately — it is a
measurement, not the gate, and it is the one place `--all-targets` is still
the right shape because nothing is being proved by it:

```bash
rustup run stable cargo llvm-cov --all-targets --all-features --locked --summary-only
```

Instrumentation does not change any test's evidence layer.

| Requirement | Process-boundary evidence |
| --- | --- |
| SQLite persistence and restart | Separate `init`, `task add`, `note`, `claim`, `handoff`, `context`, and `checkpoint` processes reopen the same board. |
| Multiple worktrees | A second directory attaches to a named board and reads/writes the same board; `workspace list` includes rootless boards and dashboard reports the roots as hints. |
| Root-resolution parity | Compiled processes prove one resolver order across mutating and read-only commands: typed `--db` / `--project` / `--workspace`, then `KANBAN_DB` / `KANBAN_PROJECT`, then cwd. Filesystem roots are canonicalized and the nearest active registered ancestor wins; rootless boards refuse cwd and `--workspace` discovery and remain reachable only by explicit identity. Adopted multi-root boards retain one board path and shared reads/writes without leaking into a registered neighbor. Repoint preserves board identity while replacing exactly the moved canonical roots; detach immediately removes discovery even when the old directory is recreated, and the final detach leaves only explicit project-name access. Retired boards refuse path, name, environment, watch, mutating, and read-only subscription selectors with the recorded retirement note instead of falling through to an active cwd board. A real local Git repository, linked sibling worktree, and local submodule prove Git topology affects provenance only: an unattached linked worktree refuses until explicitly attached, while an unregistered submodule beneath an active root resolves upward to that root. Git-specific legs emit an explicit skip note only when local Git topology setup is unavailable; the core filesystem parity assertions always run. |
| Native board adoption | Compiled processes adopt a handle-pinned, WAL-aware external snapshot into a pinned registry-owned directory, read it back through the CLI, and verify exact registered-byte hash/count, exact-root visibility, and `board_adopted` provenance. Process tests also prove missing/invalid/large source preflight creates no live root/lock/database/boards, `--as` is mandatory, a boards symlink cannot write externally, source symlink/traversal, foreign-key corruption, audit corruption, newer schema, duplicate name, concurrent live adoption, and an externally held canonical data-root lock all fail closed. Compiled-process coverage proves helper-process adoption succeeds when the reserved target descriptors are already occupied. The two serial isolated unit tests prove source-fd-equals-target and crossed-source remaps preserve the intended descriptor identities and clear `FD_CLOEXEC`; crash recovery reconciles both pre-commit and post-publish interruptions. |
| Atomic ownership | Two compiled processes race to claim one task; exactly one exit status may succeed. |
| Token-pressure handoff | The outgoing process creates the structured handoff, releases its lease, and an incoming process accepts with a different token. |
| Stale-token exclusion | The outgoing token is used for a post-handoff heartbeat and must fail. |
| Secret-safe read models | `task show` and rendered context are checked for both the literal token and the `leaseToken` field name. |
| atmux JSON import | A real source file containing epic, story, and task hierarchy is imported through the compiled CLI and the parent link is read back. |
| atmux SQLite import | A real legacy SQLite database is created and imported by a separate CLI process; duplicate insert-only import is rejected, then explicit reconciliation refreshes the existing rows and reports created/updated counts. |
| Operations | Dashboard counts, `doctor` integrity and rootless advisory reporting, multi-board backup, rootless restore, and reopening a copied board through `--db` are exercised. |
| Host-local active named-rule selectors | `hax_registry_requires_rule_retirement_before_retiring_a_named_board` proves a HAX-shaped registry cannot retire `px` while active `ONLY:px` or `EXCEPT:px` rules name it, lists blockers without mutation, and succeeds after explicit rule retirement; `hig_registry_refuses_absent_named_selectors_on_add_and_refingerprinted_import` proves a `px`-only HIG-shaped registry refuses `kanban`/`unum` adds and a correctly re-fingerprinted `ONLY:unum` transfer without writing rules or ledger rows; `doctor_reports_stale_active_selectors_without_blocking_rule_history` proves `doctor --json` reports directly injected stale state while `rule list --all` and `rule show` stay usable. The HAX test also proves archived rule inspection and `doctor --all` retired-board inspection. |
| Board-selector applicability | Compiled processes are refused by name for every selector every operation declares it discards, driven from `kanban schema --json` rather than restated; `doctor --db`, `backup --db` and the registry event trail are asserted directly, the selectors `init`, `workspace attach` and the board commands honour still resolve, and a manifest-wide sweep proves no operation exits zero while discarding a selector. Every selector the manifest does *not* list as ignored is passed a valid value on each read-only positional-free operation and must be honoured, which catches a command that refuses a selector it never declared. A separate process holds the data-root flock and proves `restore` still contends for it exclusively, and `doctor`/`backup` for it shared, when `KANBAN_DB` names a board outside the data root that the command discards. |
| Audit safety | Separate compiled processes create board and registry history, verify clean chains, and then reject edited, deleted and reordered event rows. Manifested backup/restore tests reject a substituted database, preserve a manifested rescue snapshot, keep archive continuity, and detect an intact older database against a retained newer anchor. |
| Existing-format compatibility | The binary opens a separately created `user_version=3` database matching the released TypeScript task schema without migration/export. |
| Pull routing and task graph | Separate CLI processes exercise `claim --next`, priority/dependency readiness, lane preference, role filtering, driver scope, assignee gates, and cycle rejection. |
| Handoff lane-address compatibility cutover | `handoff_create_refuses_bare_driver_lanes_but_not_non_lane_identities` runs the compiled CLI and proves `driver`, `driver-2`, and `driver-3` are refused before a row is written with the full typed-actor remedy, while `driverless`, `driver-two`, `driver-0`, and an ordinary untyped identity still create. `typed_lane_actor_accepts_only_its_matching_legacy_bare_lane_target` seeds released-format pending rows and proves a matching typed actor accepts a bare final lane segment without rewriting `toAgent`, `acceptedBy` keeps the full actor, a wrong typed lane and an untyped non-target are refused, and an already typed target remains exact. |
| Read-only scheduler inspection | A compiled `claim --candidates --project` process excludes dependency-blocked, draft-ancestor, container, leased, incompatible-assignee and driver-only rows; byte, timestamp and row-count receipts prove no board or registry write, and every returned row is then accepted by the atomic claim path. |
| Story lifecycle | Separate processes exercise planning through done, child-lane gates, epic activation, review signoff/revocation, reviewer/committer dispatch, and merge completion. |
| Related-row IDs under tag scope (`t-a3928b36`, George `a-daa231b3`) | `story_advance_names_tag_denied_child_id_without_exposing_its_row` runs the compiled CLI against managed board and tag scopes: the visible child's row is readable, the hidden child's direct read is denied, and advancing their visible story refuses with both open child IDs but no hidden title or tag; the story remains in-progress. The removed web event projection is historical (ADR-053). |
| Bounded projections | Long append-only history is rendered within the requested context bound while preserving the newest next action; generated TODO output declares SQLite authority. |
| SQLite-native RAG retrieval | Separate processes prove exact-ID top-one, the five-query paraphrase corpus, filters, cold-history opt-in, cross-board isolation, bounded cited results, explicit vector rebuild, and V12-to-V13 knowledge preservation. |
| MCP search parity | The generated `search` read tool and `search_rebuild` write tool execute the real CLI over stdio and return a cited source. |
| Cursor-native watch | Compiled-process tests cover literal-0 bootstrap, additive protocol-v1 payload and redaction, repeatable semantic predicates before the delivery limit, full normalized cursor binding, fail-closed selectors, malformed or future cursors, heartbeat delivery across read-only reopen cycles, and replay of removed subjects and historical relation targets. |
| Durable subscriptions | Separate compiled invocations migrate a pre-subscription board through declaration schema v21 to current schema v23, with subscriptions entering at v21 and dispatcher state at v22; add, list, show, pause and resume a declarative subscription; observe its audited watch event without a secret reference; reject unknown selectors, raw-value-shaped secret references and duplicate identities; verify generated read-only/list metadata; and prove a second board cannot see the row. |
| Bounded activity window | A real compiled-process `events/ev` read uses `--after START_MS --before END_MS` as a half-open `[after,before)` millisecond window, keeps SQL filters ahead of `--limit`, includes archived rows only with `--all`, and fails closed on time flags in registry/rule scope. |
| Durable subscription dispatcher | The dedicated compiled `kanban-dispatcher` process resolves each explicit board selector, targets one consumer, proves missing host-local allow-listed configuration fails at startup before the first materialization or claim, invokes a separately compiled deterministic fake adapter, clears inherited environment, injects only the named secret, and persists exact success/failure attempt state. Competing worker processes produce one claim/invocation; pause/resume is rechecked; exit, malformed, mismatched, oversized and timeout responses keep stable error codes; SIGTERM stops idle polling and a running adapter; and a real post-success/pre-ack process crash recovers after lease expiry with two invocations and durable `lease_expired` then `success` attempts, proving at-least-once rather than exactly-once delivery. This is the generic dispatcher gate, separate from the Codex queue bridge. |
| Codex queue adapter-contract | Separate compiled-process adapter-contract coverage with a fake Codex executable proves the bridge boundary only: argv selection, fixed `CODEX_HOME`, environment clearing, unrelated child acknowledgement, and an adapter-derived response. Focused unit tests prove exact version/help drift rejection, trusted path identity, and bounded input/output handling. Neither layer proves installed Codex support. |
| Codex app-server adapter-contract | Separate compiled-process adapter-contract coverage against a dependency-free fake Codex executable proves the experimental, opt-in read-only bridge boundary only: consumer `codex.app-server`, action `start-readonly-turn`, capability `start`, a child environment cleared to exactly `CODEX_HOME` and a fixed `PATH=/usr/bin:/bin` on both the probe and app-server spawns, version/help probes, private tempdir schema generation and verification, one accepted completion, and `AdapterResponse`-only stdout. Summary-view final turns may omit only a completed `userMessage` from `turn/completed.items`; `reasoning` and `agentMessage` items stay exact and fail closed. Focused unit tests prove exact version/help/schema-hash drift rejection, trusted path identity, and bounded input/output handling. The distinct HAX live smoke against installed Codex/model remains the live check for this bridge; no pass claim is made here. |
| Claude print adapter-contract | Separate compiled-process adapter-contract coverage against a dependency-free fake Claude proves consumer `claude.print`, action `start-readonly-turn`, capability `start`, exact probe/print argv, fixed cwd, empty stdin, and a child environment of exactly private `HOME` plus `PATH=/usr/bin:/bin`; strict object and array success; and fail-closed API/auth error, nonzero, stderr, mismatch, tool evidence, trailing JSON, and overflow paths. Adapter stdout on success is only `AdapterResponse`. This does not prove installed-Claude support; that requires a separately named live smoke. No active subscription ships. |
| Codex queue live smoke | The separately named HAX live smoke receipt against a separately owned idle test session in a disposable workspace is the distinct live check for installed Codex support, the exact ingress path, and one received queued message. It is a smoke receipt, not compiled-process E2E. |
| Codex app-server live smoke | The separately named 2026-09-05 HAX live smoke receipt is the distinct live check for installed-Codex support of the experimental, opt-in bridge, and the compiled fake-Codex contract test does not establish it. Invoking the host's own `dispatchers.json` binding with one structured `AdapterRequest` against installed `codex-cli 0.150.1` exited `0`, wrote only `AdapterResponse` to stdout and nothing to stderr, left the private cwd listing and the host's tmux panes byte-identical, and removed its own identity-pinned schema temp dir; a non-hex event ID is refused before Codex is reached. It is a smoke receipt, not compiled-process E2E, and no active subscription ships. |
| Sprint lifecycle and served-version close gate | The compiled-process tests `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes`, `sprint_v26_board_migrates_then_completes_a_proof_gated_lifecycle`, `sprint_parent_epic_scope_preserves_explicit_descendants_and_audits_detach`, `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides`, `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context`, and `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` exercise the production CLI and SQLite store. Together they prove planned/current/closed/abandoned lifecycle rules, explicit scope, one current sprint, scoped claims and handoff acceptance, audited overrides/detach, v26 migration, target capture at deploy start, exact `--served-version` matching at succeeded verification finish, close refusals, and atomic carry. Their deployment rows are deterministic local process fixtures; they are not evidence that a production tier served an artifact. |
| Sprint-scoped rule applicability and transfer | The compiled-process tests `sprint_scoped_rules_match_authoritative_task_sprint_and_update_atomically` and `sprint_scoped_rule_transfer_requires_destination_sprint_and_round_trips` in `tests/e2e.rs` prove authoritative task-sprint matching across claim, handoff acceptance, and context; atomic scope update/clear; destination-board sprint validation; and transfer round trips. |
| First-class sprint search | The compiled-process tests `sprint_search_is_board_scoped_fresh_rebuildable_and_exactly_cited` and `v27_board_migrates_sprint_search_with_citation_cache_identity_and_index_health` in `tests/e2e.rs` prove board-scoped sprint retrieval, lifecycle freshness, exact `kanban://BOARD/sprint/ID` citations, explicit rebuild, V27-to-V28 backfill, cache identity, and index health. |
| Restore authorization across rescue-only boards | `restore_requires_whole_board_read_for_every_rescue_only_board` in `tests/authz_bypass_matrix_e2e.rs` proves a compiled restore requires whole-board read authorization for every board available only through rescue state. |
| Deployment ledger | Separate compiled processes prove capability ownership, idempotent start, exact served-commit success, derived current release and CLI/MCP parity. |
| Deployment self-archive | A real archive process moves only old terminal non-current attempts out of default lists and hot partial indexes, keeps started and current successes hot, retains cold search and `--all` access, and proves a repeated sweep is idempotent. |
| Artifact-identity recovery (ADR-043) | Separate compiled processes prove the recovery path for an artifact whose build commit is genuinely unknown: `a_recovery_deploy_records_typed_artifact_identities_and_an_unknown_build_commit` reads the literal `unknown` back out of the board file and the words out of `show`/`list`/`current` and `schema --json`; `a_recovery_finish_refuses_a_missing_role_a_kind_mismatch_and_a_value_mismatch_by_name` proves the four named refusals and that the attempt stays open and unpublished after each; `artifact_mode_and_git_mode_refuse_each_others_flags` proves one mode per attempt at `start` and at `finish`; `the_git_deploy_path_is_unchanged_by_artifact_mode` proves the verified-Git path, its mismatch refusal and its idempotent replay are untouched; and `the_deployments_page_says_build_commit_unknown_in_words` reads the rendered pages over real HTTP. |
| access command schema (ADR-038 clause 12) | The generated manifest emits all nineteen `access` operations flag-for-flag and kind-for-kind, driven from `COMMANDS` rather than restated: `access_schema_matches_the_clause_12_grammar` pins the exact `readOnly` split (`principal show`, `principal list`, `explain`, `audit`, `enforcement show` read; the rest write), the three `ignoredSelectors` on every operation, and `scope`/`replaces` as list-valued. `kanban mcp` `tools/list` inherits the same operations as `access_*` tools. This is schema/unit evidence; no `access` operation is dispatched by this slice, and none of it is Linux `SO_PEERCRED` acceptance. |
| Managed-mode selector bypasses (ADR-038 clause 9) | `compiled_binary_refuses_each_selector_bypass_in_managed_mode` stamps a canonical `$XDG_DATA_HOME/kanban/registry.db` `managed` and proves the compiled binary refuses each of the five routes around the broker — `--db`, `--workspace`, `--project`, `KANBAN_DATA_DIR`, `KANBAN_DB`/`KANBAN_PROJECT` — by name with a non-zero exit. This is compiled-process evidence of the refusal gate only, on macOS: it does not exercise the broker socket, and it is not Linux peer-credential acceptance. |
| Direct-`--db` policy isolation (ADR-038 clause 9) | `compiled_binary_direct_db_open_writes_no_policy_decision` runs `task add --db` against a `direct` estate and then inspects the real registry: `access_audit` and `policy_events` stay empty, so a directly opened board is not reported as authorized. This is compiled-process evidence of the isolation rule, on macOS; the managed-state half (the same command failing before `open(2)`) is refused by the bypass gate above and is not separately re-proven here. |
| Backward-compatible unmanaged estate (ADR-038 clause 9) | `compiled_binary_legacy_registry_stays_unmanaged_and_keeps_working` stamps a canonical registry below `user_version` 14 whose stale `enforcement_state` row still says `managed` and proves the schema stamp wins: the estate reads `direct`, `task list --project` still answers, and the selector gate is a no-op. `compiled_binary_absent_registry_reads_as_unmanaged` proves a fresh install with no canonical registry is `direct` too. Both are compiled-process evidence, on macOS. |
| Linux-principal abstractions, UID reuse, MCP identity, empty-policy bootstrap, non-escalation, and epochs (ADR-038 clauses 1, 2, 3, 5, 6, 8, 11) | Library-level (in-process) evidence, not compiled-process: the two abstractions (`PeerCredentialsSource`, `PasswdDatabase`) deny forged credentials and divergent passwd pairs (`two_way_passwd_check_refuses_a_one_way_match`, `mint_fails_closed_when_the_passwd_pair_diverges`); `a_recycled_uid_binds_a_fresh_principal_with_no_grants` proves a recycled UID is a new principal holding nothing; `forged_actor_selector_or_env_value_cannot_change_the_resolved_principal` and `an_mcp_command_process_is_authenticated_by_its_peer_uid_not_any_client_value` prove `--as`, env, selectors and the MCP stdio pipe construct no authority; `empty_policy_allows_bootstrap_only_and_no_breakglass` proves epoch 0 permits bootstrap alone and refuses every `breakglass`; `grantor_with_write_cannot_grant_admin_on_the_same_tuple` and `grantor_cannot_grant_on_a_tuple_it_holds_nothing_at` prove clause-5 non-escalation; `a_denied_attempt_does_not_advance_the_epoch_but_a_policy_event_does` proves the epoch is bumped by policy events only, and `stale_context_is_refused_with_a_generic_denial` proves commit-time epoch/hash equality. None of this establishes Linux `SO_PEERCRED` acceptance: `only_linux_so_peercred_counts_as_managed_mode_evidence` pins that only `kernel_so_peercred` is managed-mode evidence, and the real Linux acceptance is a separately named live check on a Linux host, not run here. |
| t-dd01f2c7 - the harness is ready for a client-mounted page (chrome) | `an_async_mounted_root_is_found_by_test_id_in_real_chrome` proves the readiness seam the SPA cutover will hang on, in the same Chrome the rows above use. A loopback listener this case owns serves one shell whose body is empty and whose inline script appends `<div data-testid=app-root>` 1500ms later via `setTimeout`; both looks are taken at the instant `navigate_to` returns, where the naive `find_element` fails (and the page's own flag says it has not mounted) and `wait_for_app_root(tab, "app-root")` polls across the mount and returns the element, after which the flag reads `true` and the element's `data-testid` is the id it was given. The 1500ms is measured, not chosen: the naive look is itself two CDP round trips Chrome answers only once the new document exists, and returns 245ms after the navigation does. Every per-card selector the chrome rows use now lives in one `mod ui` in `tests/e2e.rs` - Rust-side values are its names, page expressions carry its `__NAME__` placeholders, which `js_value` splices - so a cutover retargets one module, and `decision_tab`/`list_tab`/`trusted_web_tab` share one `wait_for_shell_ready`, today the server-rendered `<main>`, which the cutover repoints at `wait_for_app_root`. Blocked half, not faked: the brief's React root served BY THE BINARY needs the SPA spec (t-eed0a923) and its bundle (t-992e40aa) and would be a product change out of scope here (ADR-047 §6), so the page under test is the case's own and what is proved is the harness half. |
| Owner gate — a blocked checkpoint that parks work on the board owner needs a card first (rule `g-74e8d80c`, layer: compiled-process CLI over the real binary — not chrome, not http) | `a_blocked_checkpoint_that_parks_work_on_the_owner_needs_a_card_first` claims a row and writes `checkpoint --state blocked --next-action "George re-logins to bootstrap"` while the task has no open attention row: the process exits non-zero, its refusal quotes that exact clause and names `kb attention raise … --kind blocking --task t-park`, the holder still holds the lease, and `context` carries no checkpoint. The same case writes a next action carrying its own double quote (`George runs the "bootstrap" script`) and requires the refusal to stay one line holding exactly the two quoted spans the sentence owns — the echoed clause, single-quoted inside, and the `<the ask>` placeholder. It then raises the card through `attention raise --task` and finds the first argv accepted, `state=blocked`, `nextAction` intact and the task `blocked`. |
| Owner gate — the false-positive guard: a record that assigns the owner nothing is written without a card (layer: compiled-process) | `a_blocked_checkpoint_that_assigns_the_owner_nothing_is_written_without_a_card` writes three blocked checkpoints on a cardless board and requires all three to land: one that parks the work on nobody (`Re-check crates.io on Monday`); one whose summary cites `per George's 2026-09-01 decision` while its next action names a lane; and one whose next action is `@:geoyws/kanban/driver runs the suite`, the lane ADDRESS this board spells with the operator's own name in it, which is work assigned to a worker rather than parked on a person. The gate reads only what a record assigns — a blocked checkpoint's `--next-action` and either record's `--blocker` values — never the narrative summary, and a match touching `:` before it or `/` on either side is a path, not a name. |
| Owner gate — a handoff blocker naming the owner needs the same card (layer: compiled-process) | `a_handoff_blocker_that_parks_work_on_the_owner_needs_a_card_first` runs `handoff create --blocker "waiting on George for the credential"` with no open row: refused by the same store gate, quoting the blocker and naming `--task t-hand`, with `handoff list --task` still empty. With the card raised the same argv creates a `pending` handoff carrying that blocker verbatim. A session handoff names no task, so there is no row for a card to hang on and nothing to require. |
| Release install proof without a serve surface (linux-x86_64 gate, `e-caeb1449`: no unit restart, no listener probe, no serve receipt) | `hig_release_script_installs_every_declared_binary_without_remote_hax_access_and_refuses_partial_activation` (layer: process) lays down every declared binary with receipts and provenance and refuses partial activation; `hig_release_script_prune_failure_keeps_index_and_committed_summary` and `hig_release_script_local_and_remote_install_guards_are_identical` hold the index and guard parity. |

Browser discovery for that gate is ordered as `KANBAN_CHROME`, then the existing platform, `PATH`, and fixed-system candidates, then the newest executable Playwright Chromium under `XDG_CACHE_HOME/ms-playwright` or `HOME/.cache/ms-playwright`. Chromium sandboxing stays enabled for non-root launches and is disabled only when the effective UID is `0`, because upstream Chrome refuses sandboxed root.

Passing library/unit tests or invoking `rust/main.rs` through an interpreter is
not E2E evidence. The gate is incomplete until the compiled executable passes
this matrix on a clean test data directory, including the real-browser path
above.

## The receipt law: a release receipt names both halves

The gate above proves the tree. A release receipt proves what is SERVING,
and that is two facts, each of which can be true while the other is false:

- **the executable identity** — the `MainPID` exe path
  (`readlink /proc/<MainPID>/exe`) and/or its sha256. WHICH binary is
  serving.
- **the bundle fingerprint** — the `bundle <sha256>` line the INSTALLED
  executable's `--version` prints, cross-checked against the package
  manifest's `bundleSha256`. WHICH operator UI that binary carries.

The operator UI is embedded by `include_bytes!` (ADR-048), so a correct exe
path proves nothing about the bytes a browser is served, and a manifest
agreeing with itself proves nothing about the process that came back from
the restart. A receipt carrying one half is half a measurement with the
other half assumed.

**The typed-kinds boundary, so nobody re-derives it.** `deploy finish
--observed` accepts only typed artifact identities, and
`ARTIFACT_IDENTITY_KINDS` (`rust/model.rs`) is exactly two kinds:
`docker-image-id` and `oci-manifest-digest` (ADR-043 §3). A bundle sha256
fits NEITHER — it digests an embedded asset table, not an image and not an
image manifest — and a `finish` offering it as either is refused by name.
So the bundle fingerprint is recorded in the deployment row's `--receipt`
text beside the executable identity, and the typed kinds are NOT extended
to hold it. A third kind would be a model change; the receipt law asks only
that both facts be recorded where the surface already supports recording
them, which for the bundle fingerprint is the receipt text.

Full statement: [ADR-044 §Amendment
2026-09-20](../adr/ADR-044-release-packaging-is-a-capability-gate-with-measured-build-provenance.md).

## Requirements trace convention

One `## Requirements trace — docs/specs/<slice>.md <SLICE>-01..<SLICE>-nn` section per specified
slice (ADR-047 §5; conventions in `docs/specs/README.md`), with these five columns.
`Requirement` is the ID alone. `Strength` is the specification's BCP 14 keyword — `MUST`,
`SHOULD` or `MAY` — never a board priority. `Layer` is one of `unit`, `chrome`, `http` or
`process`, or `none`; it is never `e2e` for an in-process test, and a `none` row says
`no e2e coverage` plainly in its Note. `Existing test` is the exact `#[test]` function name,
verified against the build with `cargo test -- --list` before the row lands. `Note` carries the
substitute wording where browser evidence is infeasible — exactly `API-level integration
asserting real database state plus tenancy isolation` — or the superseding date, or nothing.
One row per mandatory requirement; a `MAY` gets a row only if it is actually tested. The section
is updated in the same change as the specification delta it traces.

## Requirements trace — `docs/specs/sprint.md` SPRINT-01..SPRINT-39

One row per requirement, on branch `docs/t-64fb4ae7-sprint-spec` at 2026-09-19: commit
`d15822d`, where ADR-045 is `Implemented` and the specification is at `Draft — gate requested
2026-09-19`, so these rows land with it and the gate reviewer reads the trace rather than a
promise of one; `Layer` uses the specification's own vocabulary, where `process` is a
compiled-binary process-boundary exchange in `tests/e2e.rs`, `unit` is an in-process `#[test]`
in `rust/store.rs`, `rust/serve.rs` or `rust/db.rs`, and `chrome` is a compiled-binary test
driving real Chrome. Of the 39 requirements — all `MUST` — 20 are proved at `process`, 13 at
`unit` and 1 at `chrome`, and 5 carry `none` and say `no e2e coverage` plainly: `SPRINT-04`,
`SPRINT-06`, `SPRINT-09`, `SPRINT-21` and `SPRINT-28`, each an implemented refusal that no
test fails when its sentence changes; `process` and `chrome` names were enumerated with
`cargo test --locked --test e2e -- --list` and `unit` names verified as `fn <name>(` in the
source, because the lib test target does not compile in this documentation worktree.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `SPRINT-01` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | the compiled binary refuses `--id not-a-sprint` with the `must start with sp-` sentence and writes no row |
| `SPRINT-02` | MUST | unit | `a_version_must_be_semver_shaped_before_a_sprint_exists` | `rust/store.rs` `mod tests`: the validator over seven bad shapes, then `create_sprint` refusing `v1` with the `X.Y.Z` sentence quoting the value |
| `SPRINT-03` | MUST | process | `every_enum_argument_refusal_names_the_whole_set` | the `sprint-list-status` row of `ENUM_ARGUMENTS`; the same test proves `schema --json` publishes the set it names |
| `SPRINT-04` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (run `sprint new` with a negative `--start`, with an `--end` before `--start`, and with a non-integer `--start`, asserting the `sprint schedule requires non-negative --start and --end at or after --start` and `--start must be an epoch-millisecond integer` refusals). The schema `CHECK` is exercised only incidentally by the ladder test today |
| `SPRINT-05` | MUST | unit | `starting_a_second_current_sprint_is_refused_naming_the_holder` | asserts `starts_at > 0` only after a successful start; `a_planned_sprint_cannot_close_and_abandon_needs_a_note` asserts `ends_at` is set by `abandon`, and `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` asserts `closedByDeployment` after a close |
| `SPRINT-06` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (point `claim`, `task move` and `handoff create` at an `sp-` id and assert each is refused as an unknown task, leaving the sprint row untouched). The property is structural today — `sprints` has no claim, lease or gate column and no `sprint` subcommand takes a lease |
| `SPRINT-07` | MUST | unit | `starting_a_second_current_sprint_is_refused_naming_the_holder` | asserts the exact holder sentence, that the refused row stays `planned`, and that no event was appended; `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context` asserts the same refusal over the compiled binary |
| `SPRINT-08` | MUST | process | `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context` | asserts the `requires --candidate, --parent-epic, existing explicit scope, or --empty-scope` refusal; `sprint_plan_records_only_real_scope_moves_with_previous_assignment` holds the `--empty-scope` exclusivity |
| `SPRINT-09` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (run `sprint plan` with no `--body`/`--body-file`, and with a blank one, asserting the `the goal and success criteria are the plan` and `sprint body is required` refusals). Every `sprint plan` case in the suite passes a body today |
| `SPRINT-10` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | asserts the `no recorded goal and criteria` refusal on an unplanned start; the `has not been planned` and `no deliberate scope` refusals are reached only through the store today |
| `SPRINT-11` | MUST | unit | `a_planned_sprint_cannot_close_and_abandon_needs_a_note` | the `planned, not current` close refusal and the `is abandoned; open a new sprint instead of starting this one` start refusal; `starting_a_second_current_sprint_is_refused_naming_the_holder` holds the `current and cannot be replanned` refusal with the body and event count unchanged |
| `SPRINT-12` | MUST | unit | `a_planned_sprint_cannot_close_and_abandon_needs_a_note` | `abandon note is required`, then status `abandoned` with `ends_at` set and the note on the `sprint_abandoned` payload. No compiled-binary case runs `sprint abandon` |
| `SPRINT-13` | MUST | process | `every_capped_listing_refuses_a_default_it_would_exceed_and_answers_one_it_meets` | the `sprints` row of `CAPPED_LISTINGS` at default 100; the default hiding of `closed`/`abandoned` rows and the newest-first order are asserted by no test today |
| `SPRINT-14` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | reads the carry destination back with `sprint show --json` and finds the carried row; `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` reads the destination's rows and their statuses the same way |
| `SPRINT-15` | MUST | process | `sprint_parent_epic_scope_preserves_explicit_descendants_and_audits_detach` | the epic's subtree attaches, a descendant already in another sprint is preserved, and `--clear-sprint` emits one `task_sprint_changed` with a null `newSprintID`; `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` asserts the whole-command rollback when `--sprint` names a missing sprint |
| `SPRINT-16` | MUST | unit | `attaching_an_epic_carries_its_subtree_skipping_other_sprints_rows` | asserts the `abandoned; attach the rows to a planned or current sprint instead` refusal; `sprint_scoped_rule_transfer_requires_destination_sprint_and_round_trips` reaches the same gate from the carry-over destination side |
| `SPRINT-17` | MUST | unit | `claims_are_scoped_to_the_current_sprint_and_overrides_are_recorded` | the default pool is the current sprint's rows only, `--any-sprint` restores all three, a named sprint sees its own, and unknown and abandoned names are refused by their exact sentences |
| `SPRINT-18` | MUST | process | `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides` | asserts `not in sprint sp-current-boundary` and that the override flags then succeed; `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` asserts the same refusal names `--any-sprint` |
| `SPRINT-19` | MUST | process | `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides` | reads `sprintOverride` off the `task_claimed` events in `kb ev`; `claims_are_scoped_to_the_current_sprint_and_overrides_are_recorded` asserts a default-scoped claim grows no payload key |
| `SPRINT-20` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | a handoff on an out-of-boundary row is refused at acceptance naming `--any-sprint`, then accepted with it, and the accepted claim names the task |
| `SPRINT-21` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (pass `--sprint` and `--any-sprint` together on each of `claim --candidates`, `claim` and `handoff accept`, asserting the `--sprint and --any-sprint both answer which sprint boundary this claim crosses` refusal, `rust/lib.rs:2488`). No test exercises any of the three call sites today |
| `SPRINT-22` | MUST | unit | `a_board_without_a_current_sprint_claims_exactly_as_before` | a merely `planned` sprint leaves the pool and the claim payload untouched; `sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides` runs the same check over a second compiled-binary board with no sprint |
| `SPRINT-23` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | asserts the `requires --deployment` refusal |
| `SPRINT-24` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | asserts the `failed (verification)` refusal and the non-verification phase refusal verbatim; `a_sprint_closes_only_on_a_succeeded_verification_deployment` holds the same gate at `unit` |
| `SPRINT-25` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | asserts `bound to this sprint and served version 5.0.0` for an attempt bound elsewhere |
| `SPRINT-26` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | the start stores `sprintID` and `targetVersion`, a `9.9.9` finish is refused naming `target version 1.2.3`, and the matching finish succeeds |
| `SPRINT-27` | MUST | process | `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` | the `--carry-to` refusal, the `carry-over note` refusal, the destination holding the carried row at its own status, and the carry and close events with their payloads |
| `SPRINT-28` | MUST | none | `none` | no e2e coverage — owed by `e-ee95dbd5` (close a sprint whose every attached row is `done`/`cancelled` while passing `--carry-to`/`--carry-note`, and a close naming itself as the destination, asserting `has no unfinished rows to carry over` and `carry-over destination must be a different sprint`). No test closes a fully finished sprint with carry flags today |
| `SPRINT-29` | MUST | process | `sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes` | reads `currentSprint` off `dashboard --json`: target version, open, done, goal and a positive `daysRemaining`; `a_context_packet_carries_the_tasks_sprint_and_the_dash_counts_it` holds the same projection at `unit` |
| `SPRINT-30` | MUST | process | `sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context` | the attached task's JSON and text context, and the unattached task's absent field and absent `## Sprint` section; `context_packet_task_and_sprint_share_one_read_snapshot` holds the one-snapshot rule |
| `SPRINT-31` | MUST | unit | `serve_render_fixture_child_process` | reads the served bytes of `/sprints`, `/sprints/{project}` and `/sprint/{project}/{id}`: the board section, the current card's `data-sprint-*` values, the history cards' states, the archived marker and the empty-board sentence |
| `SPRINT-32` | MUST | chrome | `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records` | reaches `/sprints` from the drawer and `/sprints/{project}` from a board link at phone width; `no_route_overflows_sideways_at_three_widths_in_real_chrome` loads all three sprint routes, including a seeded `/sprint/{project}/{id}`, at 390, 820 and 1280 px |
| `SPRINT-33` | MUST | unit | `sprint_projection_counts_and_proof_obey_tag_visibility` | `sprint_projection_counts_and_proof_obey_tag_visibility` died with `rust/serve.rs` under `e-caeb1449` (served card and detail page are gone); `managed_sprint_projections_hide_restricted_rows_counts_and_event_ids` holds the same rule over the store's own projections |
| `SPRINT-34` | MUST | process | `sprint_v26_board_migrates_then_completes_a_proof_gated_lifecycle` | runs the whole lifecycle and asserts `audit verify` healthy for the registry and the board; `sprint_close_rejects_every_unqualified_proof_and_durably_records_carry` asserts the close and carry payload keys, and `sprint_parent_epic_scope_preserves_explicit_descendants_and_audits_detach` the attachment payloads |
| `SPRINT-35` | MUST | unit | `sprint_plan_records_only_real_scope_moves_with_previous_assignment` | one event per row that actually moved, each carrying its `oldSprintID` |
| `SPRINT-36` | MUST | unit | `board_schema_v27_migrates_a_real_v26_board_and_preserves_rows` | `rust/db.rs` `mod tests`; `v27_refuses_unexpected_objects_and_rolls_back_without_partial_schema` holds the fail-closed half |
| `SPRINT-37` | MUST | process | `sprint_v26_board_migrates_then_completes_a_proof_gated_lifecycle` | the compiled binary opens a real v26 board, keeps the legacy row readable, and closes a sprint on a bound verification attempt |
| `SPRINT-38` | MUST | process | `sprint_search_is_board_scoped_fresh_rebuildable_and_exactly_cited` | the `kanban://BOARD/sprint/ID` citation, freshness across lifecycle writes, board scoping and rebuild; `v27_board_migrates_sprint_search_with_citation_cache_identity_and_index_health` holds the same over a board migrated from V27 |
| `SPRINT-39` | MUST | unit | `board_schema_v28_backfills_sprints_without_rekeying_or_erasing_cached_documents` | `rust/db.rs` `mod tests`: `seq`, `source_hash` and cached embeddings survive the document-table rebuild and existing sprints are backfilled |

## Requirements trace — `docs/specs/model-restriction.md` MODEL-01..MODEL-18

One row per requirement, on branch `docs/t-1331c416-model-spec` at 2026-09-19: commit `7d1505d`,
where the specification is at `Draft — gate requested 2026-09-19` and ADR-049 is pending, so these
rows land with the specification and the gate reviewer reads the trace rather than a promise of
one. The tests these rows name do not exist at that commit: they are the fixed test set the
implementation commit lands under `t-1331c416`, and the main loop enumerates every name with
`cargo test --locked --test e2e -- --list` (for `process`) and verifies each `unit` name as
`fn <name>(` in `rust/model.rs` and `rust/serve.rs` before the `SPEC-READY` stamp — an unenumerated
name blocks the stamp. `Layer` uses the specification's own vocabulary, where `process` is a
compiled-binary process-boundary exchange in `tests/e2e.rs` and `unit` is an in-process `#[test]`.
Of the 18 requirements — all `MUST` — 15 are proved at `process` and 2 at `unit`, and 1 carries
`none` and says `no e2e coverage` plainly: `MODEL-16`. This slice has no browser evidence at all.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `MODEL-01` | MUST | unit | `a_model_name_has_one_shape` | `rust/model.rs` `mod tests`: the regex table — accepted and refused shapes, the 64-character bound, the leading-character rule, and case sensitivity |
| `MODEL-02` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | reads `allowedModels` back off `task show --json`: present on every task, sorted, empty on an unrestricted row |
| `MODEL-03` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | `task add --allowed-model` repeated, then read back as the filed row's list |
| `MODEL-04` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | the wholesale replace and the `--clear-allowed-models` empty, each read back |
| `MODEL-05` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | both flags together refused with the pair sentence, and a bad name refused, with the previous list intact |
| `MODEL-06` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | `task list --allowed-model` returns the restricted row and not the unrestricted one |
| `MODEL-07` | MUST | process | `task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags` | `allowedModels` in the `task_updated` changed-fields list when the set moved, absent when it did not |
| `MODEL-08` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | the allowed claim succeeds and `claim.model` reads the declared name; an unrestricted claim without `--model` reads `null` |
| `MODEL-09` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | the verbatim `pass --model with one of them to claim it` refusal, with the sorted set rendered, and no claim written |
| `MODEL-10` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | the verbatim `model {m} may not claim it` refusal, then the allowed model succeeding |
| `MODEL-11` | MUST | process | `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` | a driver-only restricted row refuses as driver-only, and an assigned restricted row refuses as model-restricted — the order assertion |
| `MODEL-12` | MUST | process | `claim_next_and_candidates_skip_restricted_rows_unless_the_model_matches` | the restricted row absent from `--candidates` with no model and with a non-listed one, present with a matching one, and `--next` never selecting it otherwise; unrestricted rows unaffected |
| `MODEL-13` | MUST | process | `handoff_accept_honours_the_task_model_allow_list` | acceptance refused without a matching `--model` with the identical sentence and the handoff still `pending`, then accepted with it |
| `MODEL-14` | MUST | process | `handoff_accept_honours_the_task_model_allow_list` | `model` on the `handoff_accepted` payload when given; `model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list` holds the `task_claimed` half and the absent key |
| `MODEL-15` | MUST | process | `a_v29_board_gains_task_models_and_claim_model_and_existing_claims_read_null` | a real V29 board opens at 30, gains the table, index and column, and its pre-existing claim reads `model: null`; `compiled_binary_still_migrates_a_board_that_is_behind` holds the re-run half of the same step |
| `MODEL-16` | MUST | none | `none` | no e2e coverage — owed, owner George (OQ-4): reopen a board and re-read a restricted row's list, pass the same `--allowed-model` twice and read one entry back, and remove the task and assert its `task_models` rows are gone. The property is structural today — the primary key and the `ON DELETE CASCADE` on `task_models` |
| `MODEL-17` | MUST | process | `mcp_schema_exposes_allowed_model_array_and_claim_model_string` | `schema --json` kinds `list`/`list`/`value` for `allowed-model` on `task add`/`task update`/`task list` and `value` for `model`; the MCP tools type it array on `task_add`/`task_update`, string on `task_list`, and `model` string on `claim`/`handoff accept` |
| `MODEL-18` | MUST | unit | `task_detail_lists_allowed_models_and_the_holders_model_unit` | `rust/serve.rs` `mod tests` reading served bytes: the `allowed models` row present when restricted and absent when not, and the holder's `model` line — no e2e coverage, and none planned for this row |

## Requirements trace — docs/specs/acc.md ACC-01..ACC-21

Definition authoring and schema evidence landed on 2026-09-21 for ACC-01..04, the
raiser-only authoring half of ACC-05, and the always-redacted show/list portion of ACC-13.
The answer slice landed on 2026-09-22 for the resolve half of ACC-05 (answered-open lock and
reopen-clear) and all of ACC-06/07/08 at the CLI/store layer, including the v32 result
columns. The web card landed later on 2026-09-22 for ACC-09..12, including the shared POST
endpoint and the source-redaction sweep. The miss-rate report landed on 2026-09-23 for
ACC-18/ACC-19: the CLI `--check-report` and the one `/decided` summary block, both grouped
by the shared `Store::aggregate_check_report`. The 2026-09-24 owner-authorised scope change
(`t-1aa9f553`) amended ACC-06 — a bare resolve now settles a checked row and leaves the check
pending — and landed ACC-20/ACC-21: one answer accepted open or resolved, and the CLI
`attention check` verb with its teaching receipt. Digest/reader cutover and legacy body
migration remain `PLANNED`; no row below claims those later slices.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `ACC-01` | MUST | process | `native_attention_check_round_trips_rewrites_redacts_and_refuses_atomically`; `a_complete_check_round_trips_and_every_partial_shape_names_its_missing_field`; `check_bounds_question_shape_and_answer_key_are_refused_by_field`; `v31_sqlite_refuses_both_partial_definition_directions` | compiled exact round-trip/rewrite and atomic refusal; unit partial/bound/answer-key matrix; raw SQLite all-NULL/all-non-NULL invariant in both partial directions |
| `ACC-02` | MUST | unit | `check_about_accepts_only_the_three_approved_subject_shapes` | path/file, host/tier sigil and flag/default shapes plus byte-exact mismatch sentence |
| `ACC-03` | MUST | unit/process | `check_json_refuses_decisional_and_unknown_fields`; `native_attention_check_round_trips_rewrites_redacts_and_refuses_atomically` | serde refuses outcome, recommended and unknown definition fields; serialized choices never carry them or mutate the decision |
| `ACC-04` | MUST | unit | `diagnosis_markers_are_refused_in_every_checked_text_location_with_the_exact_rule`; `diagnosis_markers_respect_case_and_word_boundaries`; `native_check_refuses_a_diagnosis_shaped_raise_then_accepts_the_rewrite` | every approved marker in question, choice label and explanation with the byte-exact rule sentence; case/word-boundary edges plus the compiled raise-refuse-then-rewrite e2e; `about` is intentionally outside this marker rule |
| `ACC-05` | MUST | process | `native_check_store_round_trip_redaction_authorization_and_atomic_update`; `native_attention_check_round_trips_rewrites_redacts_and_refuses_atomically`; `answered_check_locks_definition_and_a_later_resolve_reuses_it`; `attention_check_update_is_raiser_only_across_the_three_actors` | only `raisedBy` may author/update, including no `geoyws` exception with another lane refused the same way; the answered-open lock refuses a raiser rewrite while the answer stands; reopen clears decision and check result together and restores raiser update; resolve takes only `--check-answered`, never a check definition |
| `ACC-06` | MUST | process | `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `answered_check_locks_definition_and_a_later_resolve_reuses_it`; `attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits` | as amended 2026-09-24: a bare resolve settles a checked row with no result or `ACC:` echo and the check stays redacted on the receipt, show and resolved list; a recorded answer lets a later resolve settle with no second flag; a no-check row refuses `--check-answered` by name and otherwise resolves as before |
| `ACC-07` | MUST | process | `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `v32_result_columns_are_absent_complete_and_defined` | declared key records `answered`/`correct`/`answeredAt` as columns the v32 triggers keep absent-or-complete on a defined check; the `ACC: pass` / `ACC: miss on <key>` echo agrees with the stored fields and the `attention_resolved` payload; an undeclared key is refused before any write |
| `ACC-08` | MUST | process | `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `answered_check_locks_definition_and_a_later_resolve_reuses_it` | one right or wrong declared answer resolves with its result and echo; a second `--check-answered` after any recorded answer is refused unchanged; no attempts counter or retry path exists in the surface; concurrent submissions serialize behind the same Store transaction |
| `ACC-09` | MUST | chrome | `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | check first with the `about` chip, decision inert behind a disabled fieldset until the server holds the answer, one answer with no retry control, a miss shows the explanation above the unlocked choices |
| `ACC-10` | MUST | chrome | `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | pre-answer sweep of the rendered page and the `/api/v1/needs-you` projection: no sentinel, no `answer`, no `explanation`, no `answered` field anywhere in the bytes; the reveal arrives only with the post-answer projection |
| `ACC-11` | MUST | http | `the_check_card_answers_before_the_decision_and_never_leaks_the_key`; `answered_check_locks_definition_and_a_later_resolve_reuses_it` | the POST carries one key through the same Store operation the CLI resolve uses; the recorded answer then settles a later resolve with no second flag; the loser-of-two-submissions conflict half is exercised at store level by the one-answer refusal and remains browser-unexercised by design (one tab, one answer) |
| `ACC-12` | MUST | chrome | `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | digits answer the check pre-unlock and the decision after it, keyboard and pointer paths both decide, focus lands on the produced explanation/choice, pass and miss read as words with colour behind them, and Undo keeps working on the decided rows |
| `ACC-13` | MUST | process | PARTIAL — `native_attention_check_round_trips_rewrites_redacts_and_refuses_atomically`; `native_check_store_round_trip_redaction_authorization_and_atomic_update`; `resolve_records_the_native_check_answer_as_data_across_the_three_paths`; `the_check_card_answers_before_the_decision_and_never_leaks_the_key` | every pre-answer show/list and mutation receipt omits answer/explanation even for the raiser, and the HTTP projection sweep pins the same omission in the browser bytes; after the answer is recorded show/list carry answer, explanation and result, and a reopen redacts again; the digest projection half stays unproven — no digest test in the tree (t-0382c937, 2026-09-29) |
| `ACC-14` | MUST | process | `checked_row_stays_non_enumerating_to_an_unauthorized_actor`; `search_scores_are_a_function_of_permitted_documents_only`; `note_attention_raise_and_sitrep_refuse_a_tag_denied_task_like_an_unknown_id`; `a_removed_tasks_trail_stays_tag_gated_on_every_tail`; `removed_task_links_stay_tag_gated_on_every_listing_search_and_lane`; `an_orphaned_handoff_stays_deniable_yet_acceptable_and_archivable`; `removed_task_ids_are_never_reused_and_probe_like_live_denied_ids`; `reusing_a_task_id_is_refused_with_a_plain_message_where_no_guard_can_deny`; `compiled_binary_accepts_a_handoff_on_a_removed_task_and_archives_it`; `compiled_binary_hides_an_orphan_deployment_and_doctor_reports_it`; `store::tests::managed_deployment_listing_hides_an_orphan_link_and_doctor_reports_it`; `schema_36_keeps_task_links_without_foreign_keys`; `schema_36_backfills_pre_v36_nulled_links_from_creation_events`; `dependency_replacement_keeps_a_tag_denied_prerequisite`; `task_attach_writes_refuse_a_tag_denied_task_like_an_unknown_id`; `managed_pages_fill_past_denied_rows_with_a_true_truncation_probe`; `watch::tests::a_limit_1_follow_poll_advances_by_the_scan_floor_over_denied_rows`; `watch::tests::a_one_shot_watch_behind_denied_rows_reports_progress_not_silence`; `watch::tests::an_unenforced_one_shot_watch_stays_silent_behind_rejected_rows`; `denied_and_unknown_ids_answer_identically_on_every_by_id_attention_surface`; `denied_and_unknown_task_ids_answer_identically_on_task_routes`; `store::tests::managed_notes_checkpoints_and_named_claim_deny_denied_and_unknown_tasks_identically`; `task_linked_rows_withhold_a_tag_denied_task_on_every_listing`; `residual_lease_sprint_and_deployment_ids_answer_identically_under_enforcement`; `attention_by_id_withholds_rows_on_a_tag_denied_task`; `subscription_relation_targets_withhold_a_tag_denied_task_on_read`; `store::tests::removed_task_tag_union_fails_closed_when_a_snapshot_names_no_tags_array`; `import_requires_whole_board_write_and_names_no_denied_id`; `compiled_binary_doctor_reports_a_nulled_row_from_a_reused_live_task_id` | same-key check post, show and answering resolve on another board's checked row all receive the generic denial with no question, choice, answer, explanation or `about` anywhere, and the check stays unanswered with no result afterwards; a tag-denied document moves no permitted hit's served `lexicalScore` or `score`; A11's HTTP half stays unexercised (t-2e2ea981, t-e9c0127a); note, attention raise `--task` and sitrep post `--task` answer a denied id and a never-created id byte-identically under a managed principal, record nothing, and keep plain not-found messages unmanaged (t-d2fd604a, A21); a removed task's trail stays tag-gated on board-wide `events`, `watch --follow` and `search`, and `events --task` on the gone row refuses exactly like a never-created id (t-bd66208d, A22); a removed secret task keeps its linked rows removal-tag-gated across listings, search, lanes, watch and by-id surfaces with taskless controls readable (t-2cffbe08, A23); orphaned handoffs stay deniable yet acceptable without a lease, archivable, and doctor-healthy with orphan links reported (t-2cffbe08, A23); removed and live denied ids probe identically on `task add --id` with plain refusals unmanaged (t-2cffbe08, A24) |
| `ACC-15` | MUST | process | SUPERSEDED 2026-09-24 | Ledger + skills scope change (`t-1aa9f553`): the native-cutover wording is replaced; live behaviour is ACC-20/ACC-21 |
| `ACC-16` | MUST | process | `migrate-acc-body-blocks.sh` + `migrate-acc-body-blocks.test.sh`, wired at `scripts/release-gate.sh:93`; `schema_30_migrates_once_to_native_check_columns_without_inventing_a_check` | one-shot conversion of valid legacy `ACC:` blocks with operator receipt; rows without a block byte-for-byte unchanged; rerun migrates nothing; invalid prose reported for hand migration (t-0382c937, 2026-09-29) |
| `ACC-17` | MUST | process | SUPERSEDED 2026-09-24 | resolve-no-longer-waits (`t-1aa9f553`); live behaviour is ACC-06/ACC-20 |
| `ACC-18` | MUST | process | `att_list_check_report_groups_worst_first_with_adr037_caps`; `att_list_check_report_fans_out_across_boards`; `aggregate_check_report_groups_worst_first_with_truncation_and_skips` | A17 table values, truncation case, JSON keys, limit/cap refusals, status/filter/shape-conflict refusals and empty board; registry fan-out with the board-selector refusal; store-level worst-first, truncation and skip unit |
| `ACC-19` | MUST | chrome | `decided_page_carries_one_check_summary_block` | A18 sentence shape, row links, test ids and omission on empty, plus the `/api/v1/decided` `checkSummary` projection beside the page's rows |
| `ACC-20` | MUST | process | `attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits`; `answered_check_locks_definition_and_a_later_resolve_reuses_it` | the compiled binary answers a resolved row's check once (status and resolution unchanged, `attention_check_answered` recorded without the explanation) and an open row's check once (row stays open); identical and different second answers are refused with the board and audit chain unchanged; reopen clears and the check answers again |
| `ACC-21` | MUST | process | `attention_check_answers_once_open_or_resolved_and_resolve_no_longer_waits` | `attention check --json` receipt carries `answer`, `explanation`, `answered`, `correct`, `answeredAt`; the text receipt is byte-exact `ACC: pass` / `answer: <key> — <label>` / `why: <explanation>`; the raiser may answer, a non-raiser non-`geoyws` actor is refused naming the raiser, no-check and undeclared-key refusals write nothing |

21 requirements: 21 MUST, no SHOULD or MAY. ACC-01..12 carry definition-, answer- and
web-card-slice evidence (ACC-05 and ACC-13 remain partial pending their remaining halves);
ACC-18/ACC-19 carry the miss-rate report evidence; ACC-20/ACC-21 carry the deferred-answer and
CLI check-verb evidence; every reader/migration slice remains `PLANNED`.

## Requirements trace — docs/specs/complaint.md COMPLAINT-01..COMPLAINT-07

The sixth attention kind landed with the specification on branch
`wt-t-366502cb-spec-1790092721`, where the specification is at `SPEC-READY
2026-09-23`. Every test these rows name exists in that build, enumerated with
`cargo test --locked --test e2e -- --list`. `Layer` uses the specification's
own vocabulary, where `process` is a compiled-binary process-boundary exchange
in `tests/e2e.rs` with no HTTP and no browser. Of the 7 requirements — all
`MUST` — 7 are proved at `process`, and none carries browser evidence: the
slice changes no served markup, so there is no browser surface to drive, and
each row says `no e2e coverage` plainly.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `COMPLAINT-01` | MUST | process | `complaint_kind_raise_list_show_round_trip` | no e2e coverage |
| `COMPLAINT-02` | MUST | process | `five_legacy_kinds_unchanged_after_complaint_lands` | no e2e coverage |
| `COMPLAINT-03` | MUST | process | `unknown_attention_kind_refusal_names_all_six` | no e2e coverage |
| `COMPLAINT-04` | MUST | process | `complaint_migration_carries_five_kind_board_forward` | no e2e coverage |
| `COMPLAINT-05` | MUST | process | `complaint_resolve_reopen_matches_other_kinds` | no e2e coverage |
| `COMPLAINT-06` | MUST | process | `complaint_json_keys_match_legacy_kind_shapes` | no e2e coverage |
| `COMPLAINT-07` | MUST | process | `generated_surface_publishes_complaint_without_new_tool` | no e2e coverage |

7 requirements: 7 MUST, no SHOULD or MAY. The sixth kind raises, lists, shows,
resolves, reopens and migrates through the existing attention machinery; the
board schema stands at 35.

## Requirements trace — docs/specs/watch.md WATCH-01..WATCH-12

One row per requirement, in the `t-28dca81e` worktree at 2026-09-28: commit
`edb07459d4bf1e36ffbb18cf621a7ddc15cfd1a7` (detached at
`origin/kanban-geoyws-driver`). The specification is at `DRAFT — gate requested
2026-09-28`; these rows land with it so the gate reviewer reads the trace rather than a
promise of one. `Layer` uses the specification's own vocabulary, where `process` is a
compiled-binary process-boundary exchange with no HTTP and no browser: the slice changes no
served markup, so there is no browser surface to drive. `Existing test` is the test that
observes the behaviour today; the new surface (`--lane`, `--note-kind`, the four envelope
keys, the conformance readback) has no test yet because the implementation `t-fde5d91c`
stays gated until the specification lands, so those rows say `none` and `no e2e coverage`
plainly and name the row that must write them. Every other name was enumerated with
`cargo test --locked --test e2e -- --list` and
`cargo test --locked --test authz_bypass_matrix_e2e -- --list` on 2026-09-28 in this
worktree.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `WATCH-01` | MUST | process | `revoking_authority_stops_a_live_watch_stream_without_a_reconnect` | PARTIAL: existing live watch revocation/re-grant and exact restored event ID at the compiled-process boundary; deterministic in-process seam `watch::tests::a_poll_judges_its_snapshot_under_authority_read_after_the_snapshot` pins mint-after-snapshot ordering. No e2e coverage for the new `--lane` predicate; owed by `t-fde5d91c`. |
| `WATCH-02` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: cursor carries no lane set at the baseline. |
| `WATCH-03` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`. |
| `WATCH-04` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`. |
| `WATCH-05` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: no note-kind predicate at the baseline. |
| `WATCH-06` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: the four keys are not projected at the baseline. |
| `WATCH-07` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c` with OQ-1 readback: A6's side-by-side is the evidence. |
| `WATCH-08` | MUST | process | `none` | no e2e coverage. Owed by `t-fde5d91c`: no new cursor fields exist yet to default. |
| `WATCH-09` | MUST | process | `the_watch_surface_matches_help_and_the_mcp_manifest_excludes_it`, `watch_emits_truthful_bounded_semantic_envelopes` | additive stability on the shipped surface; `t-fde5d91c` re-runs both against envelopes carrying the four new keys. |
| `WATCH-10` | MUST | process | `watch_emits_truthful_bounded_semantic_envelopes` | the redaction half of that case; re-run with lane/type/priority-bearing events. |
| `WATCH-11` | MUST | process | `watch_follow_delivers_an_event_queued_behind_interleaved_heartbeats` | the heartbeat half; extended to lane/note-kind-skipped tails by `t-fde5d91c`. |
| `WATCH-12` | MUST | process | `watch_drains_backlogs_in_bounded_batches_and_rejects_invalid_limits`, `watch_follow_still_refuses_a_zero_limit` | limit-before/after-filtering and the at-least-1 refusal; re-run under steered predicates. |

12 requirements: 12 MUST, no SHOULD or MAY. The steered stream is a repeatable lane and
note-kind predicate bound to the opaque cursor, with an additive four-key envelope shared
field-for-field with Ord; board schema stands at 35 and no migration rides this slice.

## Requirements trace — docs/specs/cli.md CLI-01..CLI-07

`tag add` registers only namespaced tags, refused with the board's estate, and `task add
--id` is refused unless it is the kind's own shape, on branch `wt/t-7f596f45-tagns` at
2026-09-25 for `CLI-01`..`CLI-05` and on branch `wt/t-6148c0ba-idshape` at 2026-09-25 for
`CLI-06` (board row `t-6148c0ba`); the attach refusal names the board's estate form on
branch `wt/t-7f596f45-map` at 2026-09-29 for `CLI-07` (attention `a-9254741a`). Every test
these rows name exists in that build,
enumerated with `cargo test --locked --lib -- --list` (unit rows) and
`cargo test --locked --test e2e -- --list` (process rows). `Layer` uses the specification's
own vocabulary, where `unit` is an in-process Rust `#[test]` and `process` is a
compiled-binary process-boundary exchange in `tests/e2e.rs` with no HTTP and no browser.
Of the 7 requirements — all `MUST` — 6 are proved at `process` and 1 at `unit`, and none
carries browser evidence: the slice changes no served markup (the chip half is retired
with the web view, ADR-053), so each row says `no e2e coverage` plainly.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `CLI-01` | MUST | process | `tag_add_refuses_a_bare_name_with_the_boards_mapped_estate`; `tag_add_in_transact_refuses_against_the_batch_board_not_the_cwd` | table-driven over `prjx`/`ifca`, `kanban`/`geoyws`, `memberx`/`unum` and `unum-ledger`/`unum`; asserts the exact sentence, an empty `tag list` and no `tag_added` event; the batch case runs `transact --project prjx` from the `kanban` checkout and asserts the `ifca` repair with `rolledBack: true`. no e2e coverage |
| `CLI-02` | MUST | unit | `estate_for_board_maps_each_named_board_to_its_estate` | every named board plus the `unum*` rule, unmapped names, and the slashed-name exemption. no e2e coverage |
| `CLI-03` | MUST | process | `tag_add_refuses_a_bare_name_on_an_unmapped_board_with_the_estate_list_only` | asserts the estate list is carried and no single `estate/name` is suggested. no e2e coverage |
| `CLI-04` | MUST | process | `tag_add_registers_a_namespaced_tag` | `ifca/assistant` on `prjx`: registers, attaches, reads back. no e2e coverage |
| `CLI-05` | MUST | process | `tag_filters_refuse_unknown_names_exactly_as_before` | `task list`, `attention list` and rule task-tag validation refuse bare `nope` with their baseline sentences. no e2e coverage |
| `CLI-06` | MUST | process | `task_add_refuses_a_misshaped_id_with_the_kinds_expected_shape` | `bogus id!` and both wrong-kind directions refused with the exact sentence and an empty listing; `t-1234abcd` accepted; the duplicate refused as before (`task t-1234abcd already exists`). The boundary unit test `a_task_id_has_one_shape_per_kind` pins case, the rejected separators, the length bound and the empty suffix. no e2e coverage |
| `CLI-07` | MUST | process | `tag_attach_refusal_names_the_boards_estate_form` | `task add --tag assistant` on `prjx` refused with the exact `tag add ifca/assistant` repair and no row written; the named repair then registers and attaches; the unmapped board carries the estate list with the `<estate>/` placeholder and no single form. no e2e coverage |

7 requirements: 7 MUST, no SHOULD or MAY. A refused registration writes nothing; bare
legacy tags already registered stay registered and migrate with `tag rename`; rows keep
their ids and a refused `task add` writes no row and no event.

## Requirements trace — `docs/specs/linked.md` LINKED-01..LINKED-25

One row per requirement. Original 2026-09-29 baseline: lane commit `361d7e3`
(spec content merged at `3cb07ed`; code tree identical then), with a
`SPEC-READY` stamp dated 2026-09-26 for the pre-withdrawal revision and
ADR-051 `Proposed`. Current source/test line citations were refreshed after
lane commit `521c19e` on 2026-10-01. Independent reviewer
`LinkedSpecFreshReview` stamped the current revision `SPEC-READY` on
2026-10-01; this is specification readiness, not product readiness.

The 2026-09-29 LINKED-23 withdrawal was a proposed scope change; George
authorized it on 2026-09-30 (`a-53b18f9a`). The ID is reserved and its
original obligation remains in the specification as historical text. The
fresh independent review covers the 24 active MUST rows and reserved
LINKED-23; no new test coverage is claimed.

`Layer` uses the specification's own vocabulary, where `process` is a compiled-binary
process-boundary exchange with no HTTP and no browser; `chrome` named only the
withdrawn LINKED-23 and names no active LINKED requirement; `http` names no
active LINKED requirement and is kept in the specification only so the withdrawn
row stays readable. Of the 25 IDs, never reused — 24 active requirements, all
`MUST`, plus 1 withdrawn (`LINKED-23`, with the served surface per ADR-053):
24 carry `process` rows and 1 carries the withdrawn `chrome` row; 23 active rows
carry `none` and say `no e2e coverage` plainly, each owned by the
implementation row its Note names — `t-0dcbb1a9` (companions and claim scope per its
work-package title, `LINKED-01`..`LINKED-08`, `LINKED-10`..`LINKED-14`), `t-9eff9257`
(contributions and consumer integration per its work-package title, `LINKED-15`..`LINKED-21`),
or `t-db6937ba` (exposure and workflow proof per its work-package title,
`LINKED-22`, `LINKED-24`, `LINKED-25`; `LINKED-23` is withdrawn with the served
surface and owned by no implementation row). LINKED-09 carries one row naming eight
existing tests (`tests/e2e.rs:2854`, `:3375`, `:37959`, `:37706`, `:1370`, `:40424`,
`:40527`, `:15890`, in row order), verified as `fn <name>(` at the 2026-10-01
citation baseline; they prove today's gates on today's surface and are
re-run unchanged beside the new scope gate. Full
`cargo test --locked --test e2e -- --list` enumeration ran 2026-09-26 in the Linux container
(`kanban-gate:1.95-chrome-u501`, image `f542f975e2dc`, host gate slot): 439 tests, each of the
eight names present exactly once — that count is the 2026-09-26 record; the web retirement
(ADR-053) has since removed the served-surface tests, so the current tree lists fewer and no
fresh full enumeration is claimed here. No test name below is invented: where
no test observes the behaviour, the row is `none`, following the precedent `docs/specs/spa.md`
set at creation (`cd55cbc`).

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `LINKED-01` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (one stored pairing read identically from both sides) |
| `LINKED-02` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (`(boardID, id)` identity; display-name/path/token writes refused) |
| `LINKED-03` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (symmetric edges; no one-sided live exposure) |
| `LINKED-04` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (incarnation pins across rename/retire/recreate) |
| `LINKED-05` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (restart-identical reads from both sides) |
| `LINKED-06` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (unknown/denied refusals with no partial writes or leakage) |
| `LINKED-07` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (durable binding; only selected tasks claimable) |
| `LINKED-08` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (one gate across candidates, next, named, handoff, session resumption) |
| `LINKED-09` | MUST | process | `compiled_binary_allows_exactly_one_concurrent_claimer`; `claim_candidates_are_read_only_and_match_the_atomic_scheduler`; `completion_gates_apply_to_lease_taking_handoff_acceptance`; `completion_gates_track_prerequisite_lifecycle_and_cleared_dependencies`; `compiled_binary_persists_across_processes_and_rotates_handoff_lease`; `claim_next_and_candidates_skip_restricted_rows_unless_the_model_matches`; `handoff_accept_honours_the_task_model_allow_list`; `a_transacted_write_is_identical_to_the_same_write_on_its_own` | atomic ownership, read-only scheduler parity, handoff gates, lease rotation, scheduler filtering, and single-board transact identity today; all re-run unchanged beside the new scope gate |
| `LINKED-10` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (descendants and neighbours stay out until explicitly added) |
| `LINKED-11` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (revocation/claim serialization; stale-revision refusal) |
| `LINKED-12` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (append-only audited revisions; `audit verify` healthy) |
| `LINKED-13` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (frozen parks taking; revoked ends taking, keeps heartbeats) |
| `LINKED-14` | MUST | process | `none` | no e2e coverage — to be written by `t-0dcbb1a9` (triple-checked claims; authorized exit/rebind only) |
| `LINKED-15` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (append-only contributions with full identity; no short hashes) |
| `LINKED-16` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (exactly one of the four evidence roles) |
| `LINKED-17` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (merge/squash mapping retained) |
| `LINKED-18` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (exact consumer commit plus complete nested path) |
| `LINKED-19` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (moving latest and near-misses satisfy nothing) |
| `LINKED-20` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (no false joint marking; close needs all deliverables) |
| `LINKED-21` | MUST | process | `none` | no e2e coverage — to be written by `t-9eff9257` (`non-code` disposition; cross-kind refusals) |
| `LINKED-22` | MUST | process | `none` | no e2e coverage — to be written by `t-db6937ba` (CLI/MCP agreement over the real stdio server) |
| `LINKED-23` | WITHDRAWN 2026-09-30 (`a-53b18f9a`) | chrome (retired) | `none` | George withdrew the served-UI obligation after ADR-053 retired its surface; no test to be written, no implementation row; ID reserved, never reused. Original wording remains above as history. |
| `LINKED-24` | MUST | process | `none` | no e2e coverage — to be written by `t-db6937ba` (mid-flight failure, save/reopen, retry-answers-stored) |
| `LINKED-25` | MUST | process | `none` | no e2e coverage — to be written by `t-db6937ba` (companion moves no gate; two-board transact refused; ordered-pair compensation) |

25 IDs, never reused: 24 active requirements, all `MUST`, no `SHOULD` and no
`MAY`, plus 1 withdrawn (`LINKED-23`). LINKED-09 carries one row naming eight existing
tests, re-run unchanged; the remaining 23 active requirements carry `none` rows, each owned by
`t-0dcbb1a9`, `t-9eff9257`, or `t-db6937ba` as its Note states (`LINKED-23` withdrawn with the
served surface, owned by no implementation row).

## Requirements trace — `docs/specs/claim-routing.md` CLAIM-01..CLAIM-08

The claim-routing slice is implemented by row `t-8c698a02` on branch `wt/t-8c698a02-impl`
(base `448d722`), where the specification is `SPEC-READY` on 2026-09-29. `Layer` uses the
specification's vocabulary: `process` is a compiled-binary process-boundary exchange in
`tests/e2e.rs`. All 8 requirements are `MUST` and all 8 are proved at `process`; every name was
enumerated with `cargo test --test e2e -- --list`. Four tests (`CLAIM-01`, `CLAIM-04`,
`CLAIM-05`, `CLAIM-08`) fail on the baseline and pass with the change; the other four pin the
refusals and the `--allow-reassign` bypass the change must not widen. This slice changes no
served markup, so there is no browser surface to drive.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `CLAIM-01` | MUST | process | `claim_routing_treats_bare_harness_and_typed_spellings_as_one_lane` | bare, harness and typed spellings of lane `driver-2` (and trunk `driver`) see each other's rows in both directions; `driver-20`, `driver-02`, `driver-two`, `driverless` do not |
| `CLAIM-02` | MUST | process | `claim_routing_falls_back_to_exact_strings_without_a_lane` | `geoyws`, `superdriver`, `a@b@driver-2` and `@:px/px/superdriver` match only their exact string; the named claim is refused in the existing words and writes nothing |
| `CLAIM-03` | MUST | process | `claim_routing_refuses_a_typed_lane_from_another_board` | three typed forms for `driver-2` in another team or board are refused and not offered; the board is untouched |
| `CLAIM-04` | MUST | process | `claim_candidates_show_same_lane_rows_to_the_callers_own_lane` | the measured case: `claude@driver-2` is offered, and handed by `--next`, the row assigned to `@:px/px/driver-2` |
| `CLAIM-05` | MUST | process | `named_claim_takes_a_same_lane_row` | bare `driver-2` takes the typed row; the model refusal still comes before the assignee check |
| `CLAIM-06` | MUST | process | `named_claim_refuses_a_different_lane_in_the_existing_words` | `claude@driver-3`, `codex@driver`, `driver-3` and `geoyws` get `task t-lane is assigned to @:px/px/driver-2`, are not offered it by `--candidates` or `--next`, and the board is untouched |
| `CLAIM-07` | MUST | process | `allow_reassign_still_bypasses_every_assignee_spelling` | `--allow-reassign` offers and claims rows assigned in typed, harness and lane-less spellings |
| `CLAIM-08` | MUST | process | `successful_claim_stores_the_caller_string_verbatim` | after `claim --as claude@driver-2` the stored assignee reads `claude@driver-2` |

8 requirements: 8 MUST, 8 proved at `process`, no SHOULD or MAY.

## Requirements trace — `docs/specs/deploy.md` DEPLOY-01..DEPLOY-08

The DEPLOY slice is `SPEC-READY` on 2026-09-30 (commit `93103f8`) and implemented by row
`t-1220f80f` on branch `wt/t-c720eb6b-deploy`. `Layer` uses the specification's vocabulary:
`process` is a compiled-binary process-boundary exchange in `tests/e2e.rs`. All 8 requirements
are `MUST` and all 8 are proved at `process`; every name was enumerated with
`cargo test --test e2e -- --list`. Two of the four new tests
(`deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` and
`deploy_start_refuses_dev_tiers_on_hax_for_ifca_and_unmapped_boards`) fail on the baseline and
pass with the change; the other two pin the pairings the change must not move. The slice has no
browser surface.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `DEPLOY-01` | MUST | process | `deploy_start_keeps_mbp_and_hetzner_pairings_for_an_ifca_board` | the `px` board records `@_bdt` on `geoywsMBP` and `@_bd` on `geoywsMBA`; `deploy_start_refuses_a_tier_host_pair_the_canonical_table_forbids` covers an unmapped board |
| `DEPLOY-02` | MUST | process | `deploy_start_keeps_the_mbp_tier_refusal_off_hax_in_the_same_words` | `@_bdt` and `@_bd` on `hig` for board `kanban` get the baseline MBP-tier sentence, compared byte-for-byte; nothing is written |
| `DEPLOY-03` | MUST | process | `deploy_start_keeps_the_mbp_tier_refusal_off_hax_in_the_same_words` | `@_p` on `geoywsMBP` and `geoywsMBA` get the baseline Hetzner-tier sentence, compared byte-for-byte |
| `DEPLOY-04` | MUST | process | `deploy_start_keeps_mbp_and_hetzner_pairings_for_an_ifca_board` | the `px` board records `@_p` on `hax` and `@_uat` on `hig`; the existing test covers an unmapped board |
| `DEPLOY-05` | MUST | process | `deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` | boards `kanban`, `acies`, `unum` and `unum-web` record `@_bdt` and `@_bd` on `hax` |
| `DEPLOY-06` | MUST | process | `deploy_start_refuses_dev_tiers_on_hax_for_ifca_and_unmapped_boards` | boards `px`, `prjx-root` (estate ifca) and `TIERHOST` (no estate) are refused on `hax` in the two exact sentences and write no attempt; `HAX` is not `hax` |
| `DEPLOY-07` | MUST | process | `deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` | a repeated `--operation-id op-1` returns the same attempt with `idempotentReplay: true` and one attempt row |
| `DEPLOY-08` | MUST | process | `deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards` | after the `hax` attempts the board file reads `PRAGMA user_version` 36 |

8 requirements: 8 MUST, 8 proved at `process`.

## Watch coverage note

- The watch slice is coverage-driven, not count-driven.
- It covers literal-0 bootstrap, persisted opaque cursors, malformed and future
  cursor rejection, selector and scope mismatch rejection, heartbeat delivery
  across read-only reopen cycles, additive protocol-v1 payload shape and
  redaction, repeatable semantic filters, sparse matches before `--limit`,
  removed-subject replay, and historical relation-target replay.
- This matrix does not claim deployment evidence or full-suite release status.

## Requirements trace — `docs/specs/done-gate.md` DG-01..DG-18

One row per requirement, on branch `wt/t-7038c70a-impl` at the implementation
commit (dirty, uncommitted per the slice contract: no commit, branch or push
from the implementation task). `Layer` uses the specification's own
vocabulary, where `process` is a compiled-binary process-boundary exchange in
`tests/done_gate_e2e.rs` and `unit` is an in-process `#[test]`. All eighteen
requirements are `MUST` at `process`; the `process` names below are the
specification §8 plan, proved by the sibling e2e slice in
`tests/done_gate_e2e.rs` and enumerated with
`cargo test --locked --test done_gate_e2e -- --list` on integration — they do
not exist in this change, following the `MODEL` precedent of landing the
trace with the fixed test set. Every row carries `no e2e coverage` plainly:
the whole surface is CLI, so no row has browser evidence. Where an in-process
unit test in `rust/store.rs` (`mod tests`) or `rust/db.rs` (`mod tests`)
already pins the same obligation in this change, the Note names it; those
`unit` names were enumerated with `cargo test --locked --lib -- --list`
before the row landed.

| Requirement | Strength | Layer | Existing test | Note |
| --- | --- | --- | --- | --- |
| `DG-01` | MUST | process | `done_gate_refuses_done_move_without_verdict` | no e2e coverage; unit `done_gate_refuses_a_done_move_with_no_verdict_and_changes_nothing` holds the sentence-1 wording byte-exact with row, lease and event count unchanged |
| `DG-02` | MUST | process | `done_gate_rejects_incomplete_verdict_record` | no e2e coverage; unit `done_gate_empty_evidence_never_satisfies` holds the empty-evidence refusal as sentence 1, and `done_gate_opens_for_a_foreign_planner_verdict_covering_the_head` the stored five-field row surviving restart, retag-free moves and the citing move |
| `DG-03` | MUST | process | `done_gate_refuses_self_review_verdict` | no e2e coverage; unit `done_gate_refuses_self_review_but_a_foreign_closer_passes` (A3 holder-as-reviewer plus A13 writer-as-closer, then the foreign closer succeeding) and `done_gate_released_holder_is_still_the_holder_of_record` (A16) hold sentence 3 byte-exact |
| `DG-04` | MUST | process | `done_gate_requires_resolved_decision_citations` | no e2e coverage; unit `done_gate_cited_decisions_must_be_resolved_at_the_move` holds the open-refuses, resolved-opens, reopened-refuses lifecycle |
| `DG-05` | MUST | process | `done_gate_force_requires_geoyws` | no e2e coverage; unit `done_gate_override_is_geoyws_only_and_audited` holds the sentence-4 refusal for a non-`geoyws` force with the row unchanged |
| `DG-06` | MUST | process | `done_gate_override_writes_audited_event` | no e2e coverage; unit `done_gate_override_is_geoyws_only_and_audited` reads the `done_gate_override` row back: actor `geoyws`, task, prior status and the `missing-verdict` reason on the hash chain |
| `DG-07` | MUST | process | `done_gate_refuses_executor_written_verdict` | no e2e coverage; unit `done_gate_refuses_an_executor_written_verdict` holds sentence 3 for the holder-written row, nothing stored, head unmoved, the later done-move still refused |
| `DG-08` | MUST | process | `done_gate_refusals_carry_named_reasons` | no e2e coverage; the seven verbatim sentences are held byte-exact by the unit suite: 1 in `done_gate_refuses_a_done_move_with_no_verdict_and_changes_nothing`, 2 in `done_gate_stale_verdict_refuses_until_a_fresh_one_lands`, 3 in `done_gate_refuses_self_review_but_a_foreign_closer_passes`, 4 in `done_gate_override_is_geoyws_only_and_audited`, 5 and 6 in `done_gate_refuses_short_and_unattested_shas`, 7 in `done_gate_flag_defaults_off_and_audits_changes` |
| `DG-09` | MUST | process | `done_gate_off_leaves_move_unchanged` | no e2e coverage; unit `done_gate_off_and_non_done_moves_are_untouched` moves `done` with the flag absent exactly as at the baseline |
| `DG-10` | MUST | process | `done_gate_fires_only_on_done` | no e2e coverage; unit `done_gate_off_and_non_done_moves_are_untouched` moves to `review` under the gate with no verdict demanded; the writes that count as reaching `done` are enumerated in `DG-17` |
| `DG-11` | MUST | process | `done_gate_verdicts_table_is_append_only_across_migration` | no e2e coverage; unit `board_v37_adds_an_append_only_verdicts_table_and_a_rewound_rerun_keeps_rows` (`rust/db.rs`) holds the forward-only step and the rewind-rerun, and `done_gate_migration_carries_verdicts_and_overrides_forward` holds pre-gate rows verdict-free with stored rows and override events surviving a reopen |
| `DG-12` | MUST | process | `done_gate_verdict_add_verb_records_pass_only` | no e2e coverage; unit `done_gate_opens_for_a_foreign_planner_verdict_covering_the_head` records the `pass`-only row through the holder/lease write check, and `done_gate_refuses_an_executor_written_verdict` the refused half |
| `DG-13` | MUST | process | `done_gate_refuses_short_and_unattested_shas` | no e2e coverage; unit `done_gate_refuses_short_and_unattested_shas` holds sentences 5 and 6 byte-exact with nothing stored; process test `a8_done_gate_refuses_short_and_unattested_shas` drives the compiled binary; publication is the writer's `--attest-published` attestation, recorded as `publishedAttestedBy`/`publishedAttestedAt` (asserted by `a2_done_gate_verdict_add_verb_records_pass_only`); the ledger does not verify origin (George, `a-8b3467aa`) |
| `DG-14` | MUST | process | `done_gate_stale_verdict_does_not_open_gate` | no e2e coverage; unit `done_gate_stale_verdict_refuses_until_a_fresh_one_lands` holds sentence 2 byte-exact, `done_gate_head_ordering_prefers_newest_time_then_source_then_row`, `done_gate_head_tie_break_prefers_claims_then_checkpoints` and `done_gate_head_tie_within_checkpoints_prefers_the_newest_row` hold the newest-time plus tie-break ordering, `done_gate_heartbeat_at_an_unchanged_head_stales_nothing` the heartbeat and satisfy-again rules, `done_gate_no_head_opens_only_by_override` the no-head rule, and `done_gate_abbreviated_provenance_never_matches_a_full_sha` the exact-match rule |
| `DG-15` | MUST | process | `done_gate_flag_defaults_off_and_audits_changes` | no e2e coverage; unit `done_gate_flag_defaults_off_and_audits_changes` holds absent-as-off, the `geoyws` on-toggle with its `off`-to-`on` audit event, the sentence-7 refused toggle changing nothing, and the off-toggle restoring the baseline |
| `DG-16` | MUST | process | `done_gate_story_and_epic_project_done_without_verdict` | no e2e coverage; unit `done_gate_story_and_epic_moves_take_no_verdict` holds the story-projection refusal with no gate sentence and the direct epic move succeeding with no verdict |
| `DG-17` | MUST | process | `a17_done_gate_checkpoint_done_is_gated`, `a17_done_gate_task_add_done_is_refused`, `a17_done_gate_import_done_is_refused` | no e2e coverage; each refuses with DG-08 sentence 1 on the gated board with nothing written and succeeds on the ungated twin |
| `DG-18` | MUST | process | `a18_done_gate_verdict_list_hides_unreadable_evidence` | no e2e coverage; tag-scoped managed estate: the verdict row answers with writer, reviewer, SHAs and verdict intact while unreadable evidence ids are omitted, and granting the hidden tag brings the id back |
